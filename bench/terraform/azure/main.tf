terraform {
  required_version = ">= 1.0"

  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = ">= 4.0"
    }
    http = {
      source  = "hashicorp/http"
      version = ">= 3.0"
    }
    local = {
      source  = "hashicorp/local"
      version = ">= 2.0"
    }
    random = {
      source  = "hashicorp/random"
      version = ">= 3.0"
    }
    tls = {
      source  = "hashicorp/tls"
      version = ">= 4.0"
    }
  }

  backend "s3" {
    bucket       = "clowd-haus-iac-us-east-1"
    key          = "ocync/bench/azure/terraform.tfstate"
    region       = "us-east-1"
    use_lockfile = true
    encrypt      = true
  }
}

provider "azurerm" {
  subscription_id = var.subscription_id
  features {}
}

variable "subscription_id" {
  description = "Azure subscription to create the bench target in, set in terraform.tfvars or TF_VAR_subscription_id"
  type        = string
}

locals {
  # Virginia: the Azure region nearest the AWS bench instance in us-east-2.
  location = "eastus2"
  name     = "ocync-bench"

  my_ip = "${trimspace(data.http.my_ip.response_body)}/32"
}

data "http" "my_ip" {
  url = "https://checkip.amazonaws.com"
}

resource "azurerm_resource_group" "bench" {
  name     = local.name
  location = local.location
}

################################################################################
# Container Registry
# Standard: Basic throttles too early to tell behaviors apart, and Premium's
# limits matter only when tuning for Premium.
################################################################################

# Registry names are global and alphanumeric only.
resource "random_string" "registry" {
  length  = 6
  special = false
  upper   = false
}

resource "azurerm_container_registry" "bench" {
  name                = "ocyncbench${random_string.registry.result}"
  resource_group_name = azurerm_resource_group.bench.name
  location            = azurerm_resource_group.bench.location
  sku                 = "Standard"
}

################################################################################
# Virtual machine
# Runs ocync as its system-assigned managed identity through the instance
# metadata service, the path a user on Azure takes.
################################################################################

resource "azurerm_virtual_network" "bench" {
  name                = local.name
  resource_group_name = azurerm_resource_group.bench.name
  location            = azurerm_resource_group.bench.location
  address_space       = ["10.20.0.0/16"]
}

resource "azurerm_subnet" "bench" {
  name                 = local.name
  resource_group_name  = azurerm_resource_group.bench.name
  virtual_network_name = azurerm_virtual_network.bench.name
  address_prefixes     = ["10.20.0.0/24"]
}

resource "azurerm_network_security_group" "bench" {
  name                = local.name
  resource_group_name = azurerm_resource_group.bench.name
  location            = azurerm_resource_group.bench.location

  security_rule {
    name                       = "ssh"
    priority                   = 100
    direction                  = "Inbound"
    access                     = "Allow"
    protocol                   = "Tcp"
    source_port_range          = "*"
    destination_port_range     = "22"
    source_address_prefix      = local.my_ip
    destination_address_prefix = "*"
  }
}

resource "azurerm_public_ip" "bench" {
  name                = local.name
  resource_group_name = azurerm_resource_group.bench.name
  location            = azurerm_resource_group.bench.location
  allocation_method   = "Static"
  sku                 = "Standard"
}

resource "azurerm_network_interface" "bench" {
  name                = local.name
  resource_group_name = azurerm_resource_group.bench.name
  location            = azurerm_resource_group.bench.location

  ip_configuration {
    name                          = "primary"
    subnet_id                     = azurerm_subnet.bench.id
    private_ip_address_allocation = "Dynamic"
    public_ip_address_id          = azurerm_public_ip.bench.id
  }
}

resource "azurerm_network_interface_security_group_association" "bench" {
  network_interface_id      = azurerm_network_interface.bench.id
  network_security_group_id = azurerm_network_security_group.bench.id
}

resource "tls_private_key" "ssh" {
  algorithm = "ED25519"
}

resource "local_sensitive_file" "ssh_key" {
  filename        = "${path.module}/.ssh-key"
  content         = tls_private_key.ssh.private_key_openssh
  file_permission = "0600"
}

resource "azurerm_linux_virtual_machine" "bench" {
  name                  = local.name
  resource_group_name   = azurerm_resource_group.bench.name
  location              = azurerm_resource_group.bench.location
  size                  = "Standard_B2s"
  admin_username        = "bench"
  network_interface_ids = [azurerm_network_interface.bench.id]

  admin_ssh_key {
    username   = "bench"
    public_key = tls_private_key.ssh.public_key_openssh
  }

  os_disk {
    caching              = "ReadWrite"
    storage_account_type = "Standard_LRS"
  }

  source_image_reference {
    publisher = "Canonical"
    offer     = "ubuntu-24_04-lts"
    sku       = "server"
    version   = "latest"
  }

  identity {
    type = "SystemAssigned"
  }
}

resource "azurerm_role_assignment" "bench_push" {
  scope                = azurerm_container_registry.bench.id
  role_definition_name = "AcrPush"
  principal_id         = azurerm_linux_virtual_machine.bench.identity[0].principal_id
  # Skips the directory lookup that fails while a new identity is still
  # replicating.
  principal_type = "ServicePrincipal"
}
