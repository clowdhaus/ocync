output "registry" {
  description = "Registry host to push to"
  value       = azurerm_container_registry.bench.login_server
}

output "connect" {
  description = "SSH to the virtual machine"
  value       = "ssh -i ${abspath(local_sensitive_file.ssh_key.filename)} bench@${azurerm_public_ip.bench.ip_address}"
}

output "tenant_id" {
  description = "Entra tenant the AWS bench instance signs in to (AZURE_TENANT_ID)"
  value       = data.azurerm_client_config.current.tenant_id
}

output "client_id" {
  description = "Application the AWS bench instance signs in as (AZURE_CLIENT_ID)"
  value       = azuread_application.bench.client_id
}
