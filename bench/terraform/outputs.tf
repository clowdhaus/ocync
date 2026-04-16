output "instance_id" {
  description = "EC2 instance ID for SSM session"
  value       = module.ec2.id
}

output "connect" {
  description = "SSM command to connect"
  value       = "aws ssm start-session --target ${module.ec2.id} --region us-east-1"
}
