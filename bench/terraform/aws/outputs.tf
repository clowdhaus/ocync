output "public_ip" {
  description = "Public IP of the bench instance"
  value       = module.ec2.public_ip
}

output "connect" {
  description = "SSH command to connect"
  value       = "ssh ec2-user@${module.ec2.public_ip}"
}

output "token_issuer" {
  description = "Issuer of the tokens STS signs for this account, which the Azure federated credential trusts"
  value       = aws_iam_outbound_web_identity_federation.this.issuer_identifier
}

output "instance_role_arn" {
  description = "Role the bench instance runs as"
  value       = module.ec2.iam_role_arn
}
