output "registry" {
  description = "Registry host to push to"
  value       = azurerm_container_registry.bench.login_server
}

output "connect" {
  description = "SSH to the virtual machine"
  value       = "ssh -i ${abspath(local_sensitive_file.ssh_key.filename)} bench@${azurerm_public_ip.bench.ip_address}"
}
