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
    local = {
      source  = "hashicorp/local"
      version = ">= 2.0"
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

locals {
  project_id = "ocync-513014"
  # Columbus, Ohio: beside the AWS bench instance in us-east-2.
  region = "us-east5"
  name   = "ocync-bench"

  pool_name = google_iam_workload_identity_pool.bench.name
  audience  = "//iam.googleapis.com/${google_iam_workload_identity_pool_provider.aws.name}"
}

################################################################################
# Data sources
################################################################################

data "aws_caller_identity" "current" {}

data "google_project" "current" {}

################################################################################
# APIs
################################################################################

resource "google_project_service" "this" {
  for_each = toset([
    "artifactregistry.googleapis.com",
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

################################################################################
# Service account
# What the AWS bench instance acts as, with push and pull on the repository.
################################################################################

resource "google_service_account" "bench" {
  account_id   = local.name
  display_name = "ocync bench"

  depends_on = [google_project_service.this]
}

resource "google_artifact_registry_repository_iam_member" "bench_writer" {
  repository = google_artifact_registry_repository.bench.name
  location   = google_artifact_registry_repository.bench.location
  role       = "roles/artifactregistry.writer"
  member     = google_service_account.bench.member
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
