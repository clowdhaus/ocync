terraform {
  required_version = ">= 1.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.0"
    }
  }

  backend "s3" {
    bucket       = "clowd-haus-iac-us-east-1"
    key          = "ocync/bench/terraform.tfstate"
    region       = "us-east-1"
    use_lockfile = true
    encrypt      = true
  }
}

provider "aws" {
  region = "us-east-1"
}

locals {
  instance_type   = "c6in.large"
  ebs_volume_size = 100
  ebs_iops        = 6000
  ebs_throughput  = 400 # MB/s
}

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

# ── Tags ─────────────────────────────────────────────────────────────────────

module "tags" {
  source  = "clowdhaus/tags/aws"
  version = "~> 2.0"

  environment = "dev"
  repository  = "https://github.com/clowdhaus/ocync"
}

# ── VPC ──────────────────────────────────────────────────────────────────────

module "vpc" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "~> 6.0"

  name = "ocync-bench"
  cidr = "10.0.0.0/16"

  azs             = slice(data.aws_availability_zones.available.names, 0, 2)
  private_subnets = ["10.0.1.0/24", "10.0.2.0/24"]
  public_subnets  = ["10.0.101.0/24", "10.0.102.0/24"]

  enable_nat_gateway = true
  single_nat_gateway = true

  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = module.tags.tags
}

# ── VPC Endpoints ─────────────────────────────────────────────────────────────

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
      subnet_ids          = module.vpc.private_subnets
      private_dns_enabled = true
      tags                = { Name = "ocync-bench-ecr-api" }
    }
    ecr_dkr = {
      service             = "ecr.dkr"
      service_type        = "Interface"
      subnet_ids          = module.vpc.private_subnets
      private_dns_enabled = true
      tags                = { Name = "ocync-bench-ecr-dkr" }
    }
    s3 = {
      service         = "s3"
      service_type    = "Gateway"
      route_table_ids = module.vpc.private_route_table_ids
      tags            = { Name = "ocync-bench-s3" }
    }
  }

  tags = module.tags.tags
}

# ── IAM policy for SSM parameter read ────────────────────────────────────────

data "aws_caller_identity" "current" {}

resource "aws_iam_policy" "bench_ssm_params" {
  name        = "ocync-bench-ssm-params"
  description = "Allow reading benchmark SSM parameters"

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["ssm:GetParameter", "ssm:GetParameters"]
      Resource = "arn:aws:ssm:us-east-1:${data.aws_caller_identity.current.account_id}:parameter/ocync/bench/*"
    }]
  })

  tags = module.tags.tags
}

# ── EC2 Instance ──────────────────────────────────────────────────────────────

module "ec2" {
  source  = "terraform-aws-modules/ec2-instance/aws"
  version = "~> 6.0"

  name = "ocync-bench"

  ami           = data.aws_ami.al2023.id
  instance_type = local.instance_type
  subnet_id     = module.vpc.private_subnets[0]

  # IAM instance profile with ECR full access, SSM, and parameter read
  create_iam_instance_profile = true
  iam_role_policies = {
    AmazonEC2ContainerRegistryFullAccess = "arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryFullAccess"
    AmazonSSMManagedInstanceCore         = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
    BenchSSMParams                       = aws_iam_policy.bench_ssm_params.arn
  }

  # Security group — all egress (pulls from public registries, pushes to ECR)
  create_security_group = true
  security_group_egress_rules = {
    all_out = {
      cidr_ipv4   = "0.0.0.0/0"
      ip_protocol = "-1"
    }
  }

  # gp3 root volume with provisioned performance.
  # Note: the v6 ec2-instance module uses `size` and `type` (not `volume_size`
  # and `volume_type`) — the latter are silently ignored by the module.
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

  user_data                   = file("${path.module}/user-data.sh")
  user_data_replace_on_change = true

  tags = module.tags.tags
}
