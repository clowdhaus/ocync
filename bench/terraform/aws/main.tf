terraform {
  required_version = ">= 1.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.0"
    }
    http = {
      source  = "hashicorp/http"
      version = ">= 3.0"
    }
  }

  backend "s3" {
    bucket       = "clowd-haus-iac-us-east-1"
    key          = "ocync/bench/aws/terraform.tfstate"
    region       = "us-east-1"
    use_lockfile = true
    encrypt      = true
  }
}

provider "aws" {
  region = "us-east-1"
}

locals {
  instance_type   = "c6in.4xlarge"
  ebs_volume_size = 256
  ebs_iops        = 6000
  ebs_throughput  = 400 # MB/s
  my_ip           = "${trimspace(data.http.my_ip.response_body)}/32"
  ssh_public_key  = file("~/.ssh/id_ed25519.pub")
  ssh_private_key = file("~/.ssh/id_ed25519")
}

################################################################################
# Operator IP
################################################################################

data "http" "my_ip" {
  url = "https://checkip.amazonaws.com"
}

################################################################################
# Data sources
################################################################################

data "aws_availability_zones" "available" {
  state = "available"
}

data "aws_ami" "al2023" {
  most_recent = true
  owners      = ["amazon"]

  filter {
    name   = "name"
    values = ["al2023-ami-2023.*-x86_64"]
  }

  filter {
    name   = "architecture"
    values = ["x86_64"]
  }
}

data "aws_caller_identity" "current" {}

################################################################################
# SSM Parameters (Docker Hub credentials)
################################################################################

data "aws_ssm_parameter" "dockerhub_username" {
  name            = "/ocync/bench/dockerhub-username"
  with_decryption = true
}

data "aws_ssm_parameter" "dockerhub_token" {
  name            = "/ocync/bench/dockerhub-access-token"
  with_decryption = true
}

################################################################################
# Tags
################################################################################

module "tags" {
  source  = "clowdhaus/tags/aws"
  version = "~> 2.0"

  environment = "dev"
  repository  = "https://github.com/clowdhaus/ocync"
}

################################################################################
# SSH Key Pair
################################################################################

resource "aws_key_pair" "bench" {
  key_name_prefix = "ocync-bench-"
  public_key      = local.ssh_public_key

  tags = module.tags.tags
}

################################################################################
# VPC
################################################################################

module "vpc" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "~> 6.0"

  name = "ocync-bench"
  cidr = "10.0.0.0/16"

  azs            = slice(data.aws_availability_zones.available.names, 0, 2)
  public_subnets = ["10.0.1.0/24", "10.0.2.0/24"]

  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = module.tags.tags
}

################################################################################
# VPC Endpoints
################################################################################

module "vpc_endpoints" {
  source  = "terraform-aws-modules/vpc/aws//modules/vpc-endpoints"
  version = "~> 6.0"

  vpc_id = module.vpc.vpc_id

  create_security_group      = true
  security_group_name        = "ocync-bench-vpc-endpoints"
  security_group_description = "Allow HTTPS from within the VPC to interface endpoints"
  security_group_rules = {
    ingress_https = {
      cidr_blocks = [module.vpc.vpc_cidr_block]
    }
  }

  endpoints = {
    ecr_api = {
      service             = "ecr.api"
      service_type        = "Interface"
      subnet_ids          = module.vpc.public_subnets
      private_dns_enabled = true
      tags                = { Name = "ocync-bench-ecr-api" }
    }
    ecr_dkr = {
      service             = "ecr.dkr"
      service_type        = "Interface"
      subnet_ids          = module.vpc.public_subnets
      private_dns_enabled = true
      tags                = { Name = "ocync-bench-ecr-dkr" }
    }
    s3 = {
      service         = "s3"
      service_type    = "Gateway"
      route_table_ids = module.vpc.public_route_table_ids
      tags            = { Name = "ocync-bench-s3" }
    }
  }

  tags = module.tags.tags
}

################################################################################
# IAM
################################################################################

resource "aws_iam_policy" "bench" {
  name        = "ocync-bench"
  description = "Instance metadata and ECR Public auth for benchmark reporting"

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["ec2:DescribeInstanceTypes"]
        Resource = "*"
      },
      {
        # ECR Public auth requires both ecr-public:GetAuthorizationToken
        # and sts:GetServiceBearerToken. The managed policy
        # AmazonElasticContainerRegistryPublicReadOnly does not include
        # the STS action, so we add it explicitly.
        Effect   = "Allow"
        Action   = ["sts:GetServiceBearerToken"]
        Resource = "*"
      },
    ]
  })

  tags = module.tags.tags
}

################################################################################
# EC2 Instance
################################################################################

module "ec2" {
  source  = "terraform-aws-modules/ec2-instance/aws"
  version = "~> 6.0"

  name = "ocync-bench"

  ami           = data.aws_ami.al2023.id
  instance_type = local.instance_type
  subnet_id     = module.vpc.public_subnets[0]
  key_name      = aws_key_pair.bench.key_name

  associate_public_ip_address = true

  # IAM instance profile with ECR full access and instance metadata
  create_iam_instance_profile = true
  iam_role_policies = {
    AmazonEC2ContainerRegistryFullAccess           = "arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryFullAccess"
    AmazonElasticContainerRegistryPublicReadOnly    = "arn:aws:iam::aws:policy/AmazonElasticContainerRegistryPublicReadOnly"
    Bench                                          = aws_iam_policy.bench.arn
  }

  # Security group -- SSH from operator IP, all egress
  create_security_group = true
  security_group_ingress_rules = {
    ssh = {
      cidr_ipv4   = local.my_ip
      from_port   = 22
      to_port     = 22
      ip_protocol = "tcp"
    }
  }
  security_group_egress_rules = {
    all_out = {
      cidr_ipv4   = "0.0.0.0/0"
      ip_protocol = "-1"
    }
  }

  # gp3 root volume with provisioned performance
  root_block_device = {
    type                  = "gp3"
    size                  = local.ebs_volume_size
    iops                  = local.ebs_iops
    throughput            = local.ebs_throughput
    delete_on_termination = true
    encrypted             = true
  }

  # IMDSv2 required
  metadata_options = {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  user_data = templatefile("${path.module}/user-data.sh", {
    ssh_private_key    = local.ssh_private_key
    dockerhub_username = data.aws_ssm_parameter.dockerhub_username.value
    dockerhub_token    = data.aws_ssm_parameter.dockerhub_token.value
    account_id         = data.aws_caller_identity.current.account_id
  })
  user_data_replace_on_change = true

  tags = module.tags.tags
}

################################################################################
# bench.json
# Passes structured connection data from Terraform to the xtask CLI.
# Ephemeral by design -- removed on `terraform destroy` since the
# instance (and its IP) no longer exist.
################################################################################

resource "local_file" "bench_json" {
  filename = "${path.module}/bench.json"
  content = jsonencode({
    provider = "aws"
    host     = module.ec2.public_ip
    user     = "ec2-user"
  })
}
