output "repository" {
  description = "Registry path to push to"
  value       = "${local.region}-docker.pkg.dev/${local.project_id}/${google_artifact_registry_repository.bench.repository_id}"
}

output "service_account" {
  description = "Service account the bench instance impersonates"
  value       = google_service_account.bench.email
}

output "credentials_file" {
  description = "Federation configuration for GOOGLE_APPLICATION_CREDENTIALS on the AWS bench instance"
  value       = abspath(local_file.credentials.filename)
}

output "mirror_repository" {
  description = "Second registry path, for mounts between repositories"
  value       = "${local.region}-docker.pkg.dev/${local.project_id}/${google_artifact_registry_repository.mirror.repository_id}"
}

output "connect" {
  description = "SSH to the Compute Engine instance"
  value       = "ssh -i ${abspath(local_sensitive_file.ssh_key.filename)} bench@${google_compute_instance.bench.network_interface[0].access_config[0].nat_ip}"
}
