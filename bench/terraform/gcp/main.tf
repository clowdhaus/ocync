terraform {
  required_version = ">= 1.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.0"
    }
    google = {
      source  = "hashicorp/google"
      version = ">= 7.0"
    }
    http = {
      source  = "hashicorp/http"
      version = ">= 3.0"
    }
    local = {
      source  = "hashicorp/local"
      version = ">= 2.0"
    }
    tls = {
      source  = "hashicorp/tls"
      version = ">= 4.0"
    }
  }

  backend "s3" {
    bucket       = "clowd-haus-iac-us-east-1"
    key          = "ocync/bench/gcp/terraform.tfstate"
    region       = "us-east-1"
    use_lockfile = true
    encrypt      = true
  }
}

provider "aws" {
  region = "us-east-2"
}

provider "google" {
  project = local.project_id
  region  = local.region
}

variable "project_id" {
  description = "Google Cloud project to create the bench target in, set in terraform.tfvars or TF_VAR_project_id"
  type        = string
}

locals {
  project_id = var.project_id
  # Columbus, Ohio: beside the AWS bench instance in us-east-2.
  region = "us-east5"
  name   = "ocync-bench"

  pool_name = google_iam_workload_identity_pool.bench.name
  audience  = "//iam.googleapis.com/${google_iam_workload_identity_pool_provider.aws.name}"

  my_ip = "${trimspace(data.http.my_ip.response_body)}/32"
}

################################################################################
# Data sources
################################################################################

data "aws_caller_identity" "current" {}

data "google_project" "current" {}

data "http" "my_ip" {
  url = "https://checkip.amazonaws.com"
}

################################################################################
# APIs
################################################################################

resource "google_project_service" "this" {
  for_each = toset([
    "artifactregistry.googleapis.com",
    "compute.googleapis.com",
    "iam.googleapis.com",
    "iamcredentials.googleapis.com",
    "sts.googleapis.com",
  ])

  service = each.value

  # Turning an API off on destroy breaks anything else in the project using it.
  disable_on_destroy = false
}

################################################################################
# Artifact Registry
################################################################################

resource "google_artifact_registry_repository" "bench" {
  repository_id = local.name
  location      = local.region
  format        = "DOCKER"
  description   = "ocync benchmark and verification target"

  depends_on = [google_project_service.this]
}

# A second repository, so a mount between two repositories can be tested.
resource "google_artifact_registry_repository" "mirror" {
  repository_id = "${local.name}-mirror"
  location      = local.region
  format        = "DOCKER"
  description   = "ocync cross-repository mount target"

  depends_on = [google_project_service.this]
}

################################################################################
# Service account
# What the AWS bench instance (through federation) and the Compute Engine
# instance (through the metadata server) act as, with push and pull on both
# repositories.
################################################################################

resource "google_service_account" "bench" {
  account_id   = local.name
  display_name = "ocync bench"

  depends_on = [google_project_service.this]
}

resource "google_artifact_registry_repository_iam_member" "bench_writer" {
  for_each = {
    bench  = google_artifact_registry_repository.bench
    mirror = google_artifact_registry_repository.mirror
  }

  repository = each.value.name
  location   = each.value.location
  role       = "roles/artifactregistry.writer"
  member     = google_service_account.bench.member
}

moved {
  from = google_artifact_registry_repository_iam_member.bench_writer
  to   = google_artifact_registry_repository_iam_member.bench_writer["bench"]
}

################################################################################
# Workload Identity Federation from AWS
# The pool trusts this AWS account. Only the bench instance's role, whose
# name the EC2 module prefixes with `ocync-bench`, may impersonate the
# service account.
################################################################################

resource "google_iam_workload_identity_pool" "bench" {
  workload_identity_pool_id = local.name
  display_name              = "ocync bench"

  depends_on = [google_project_service.this]
}

resource "google_iam_workload_identity_pool_provider" "aws" {
  workload_identity_pool_id          = google_iam_workload_identity_pool.bench.workload_identity_pool_id
  workload_identity_pool_provider_id = "aws-bench"
  display_name                       = "AWS bench account"

  aws {
    account_id = data.aws_caller_identity.current.account_id
  }

  # Google's documented mapping for AWS: the role of an assumed-role ARN,
  # without its session name.
  attribute_mapping = {
    "google.subject"     = "assertion.arn"
    "attribute.aws_role" = "assertion.arn.contains('assumed-role') ? assertion.arn.extract('{account_arn}assumed-role/') + 'assumed-role/' + assertion.arn.extract('assumed-role/{role_name}/') : assertion.arn"
  }

  attribute_condition = "attribute.aws_role.startsWith('arn:aws:sts::${data.aws_caller_identity.current.account_id}:assumed-role/${local.name}')"
}

resource "google_service_account_iam_member" "bench_federation" {
  service_account_id = google_service_account.bench.name
  role               = "roles/iam.workloadIdentityUser"
  member             = "principalSet://iam.googleapis.com/${local.pool_name}/*"
}

################################################################################
# gcp-credentials.json
# The federation configuration ocync reads through
# GOOGLE_APPLICATION_CREDENTIALS on the bench instance. It holds no key: only
# the provider to exchange with and the service account to impersonate.
################################################################################

resource "local_file" "credentials" {
  filename        = "${path.module}/gcp-credentials.json"
  file_permission = "0644"
  content = jsonencode({
    type                              = "external_account"
    audience                          = local.audience
    subject_token_type                = "urn:ietf:params:aws:token-type:aws4_request"
    token_url                         = "https://sts.googleapis.com/v1/token"
    service_account_impersonation_url = "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/${google_service_account.bench.email}:generateAccessToken"
    credential_source = {
      environment_id                 = "aws1"
      region_url                     = "http://169.254.169.254/latest/meta-data/placement/availability-zone"
      url                            = "http://169.254.169.254/latest/meta-data/iam/security-credentials"
      regional_cred_verification_url = "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15"
      imdsv2_session_token_url       = "http://169.254.169.254/latest/api/token"
    }
  })
}

################################################################################
# Compute Engine instance
# Runs ocync as the bench service account through the metadata server, the
# path a user on Compute Engine or GKE takes.
################################################################################

resource "google_compute_network" "bench" {
  name                    = local.name
  auto_create_subnetworks = false

  depends_on = [google_project_service.this]
}

resource "google_compute_subnetwork" "bench" {
  name          = local.name
  network       = google_compute_network.bench.id
  region        = local.region
  ip_cidr_range = "10.10.0.0/24"
}

resource "google_compute_firewall" "ssh" {
  name          = "${local.name}-ssh"
  network       = google_compute_network.bench.id
  source_ranges = [local.my_ip]

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}

resource "tls_private_key" "ssh" {
  algorithm = "ED25519"
}

resource "local_sensitive_file" "ssh_key" {
  filename        = "${path.module}/.ssh-key"
  content         = tls_private_key.ssh.private_key_openssh
  file_permission = "0600"
}

resource "google_compute_instance" "bench" {
  name         = local.name
  machine_type = "e2-standard-2"
  zone         = "${local.region}-a"

  # A change to the machine type or service account stops the instance
  # rather than failing the apply.
  allow_stopping_for_update = true

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
      size  = 20
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.bench.id
    access_config {}
  }

  service_account {
    email  = google_service_account.bench.email
    scopes = ["cloud-platform"]
  }

  # Only this key logs in: the instance hands out the service account's
  # tokens, so project-wide keys and OS Login users are kept out.
  metadata = {
    ssh-keys               = "bench:${tls_private_key.ssh.public_key_openssh}"
    block-project-ssh-keys = "TRUE"
    enable-oslogin         = "FALSE"
  }
}
