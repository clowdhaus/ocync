output "public_ip" {
  description = "Public IP of the bench instance"
  value       = module.ec2.public_ip
}

output "connect" {
  description = "SSH command to connect"
  value       = "ssh ec2-user@${module.ec2.public_ip}"
}
