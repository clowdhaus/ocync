output "repository" {
  description = "Registry path to push to"
  value       = "${local.region}-docker.pkg.dev/${local.project_id}/${google_artifact_registry_repository.bench.repository_id}"
}

output "service_account" {
  description = "Service account the bench instance impersonates"
  value       = google_service_account.bench.email
}

output "credentials_file" {
  description = "Federation configuration for GOOGLE_APPLICATION_CREDENTIALS on the bench instance"
  value       = abspath(local_file.credentials.filename)
}
