resource "helm_release" "metrics-server" {
  count       = var.enable_k8s_metrics_server || var.enable_cluster_autoscaler ? 1 : 0
  name        = "metrics-server"
  namespace   = "kube-system"
  chart       = "${path.module}/../helm/k8s-metrics"
  max_history = 10
  wait        = false

  values = [
    jsonencode({
      coredns = {
        maxReplicas = var.num_validators
      }
      autoscaler = {
        enabled     = var.enable_cluster_autoscaler
        clusterName = data.aws_eks_cluster.aptos.name
        image = {
          # EKS does not report patch version
          tag = "v${data.aws_eks_cluster.aptos.version}.0"
        }
        serviceAccount = {
          annotations = {
            "eks.amazonaws.com/role-arn" = aws_iam_role.cluster-autoscaler[0].arn
          }
        }
      }
    })
  ]
}


# access control
data "aws_iam_policy_document" "cluster-autoscaler-assume-role" {
  count = var.enable_cluster_autoscaler ? 1 : 0
  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]

    principals {
      type = "Federated"
      identifiers = [
        "arn:aws:iam::${data.aws_caller_identity.current.account_id}:oidc-provider/${module.validator.oidc_provider}"
      ]
    }

    condition {
      test     = "StringEquals"
      variable = "${module.validator.oidc_provider}:sub"
      # the name of the kube-system cluster-autoscaler service account
      values = ["system:serviceaccount:kube-system:cluster-autoscaler"]
    }

    condition {
      test     = "StringEquals"
      variable = "${module.validator.oidc_provider}:aud"
      values   = ["sts.amazonaws.com"]
    }
  }
}

data "aws_iam_policy_document" "cluster-autoscaler" {
  count = var.enable_cluster_autoscaler ? 1 : 0

  statement {
    sid = "Autoscaling"
    actions = [
      "autoscaling:SetDesiredCapacity",
      "autoscaling:TerminateInstanceInAutoScalingGroup"
    ]
    resources = ["*"]
    condition {
      test     = "StringEquals"
      variable = "aws:ResourceTag/k8s.io/cluster-autoscaler/${data.aws_eks_cluster.aptos.name}"
      values   = ["owned"]
    }
  }

  statement {
    sid = "DescribeAutoscaling"
    actions = [
      "autoscaling:DescribeAutoScalingInstances",
      "autoscaling:DescribeAutoScalingGroups",
      "ec2:DescribeLaunchTemplateVersions",
      "autoscaling:DescribeTags",
      "autoscaling:DescribeLaunchConfigurations"
    ]
    resources = ["*"]
  }
}

resource "aws_iam_role" "cluster-autoscaler" {
  count                = var.enable_cluster_autoscaler ? 1 : 0
  name                 = "aptos-testnet-${terraform.workspace}-cluster-autoscaler"
  path                 = var.iam_path
  permissions_boundary = var.permissions_boundary_policy
  assume_role_policy   = data.aws_iam_policy_document.cluster-autoscaler-assume-role[0].json
}

resource "aws_iam_role_policy" "cluster-autoscaler" {
  count  = var.enable_cluster_autoscaler ? 1 : 0
  name   = "Helm"
  role   = aws_iam_role.cluster-autoscaler[0].name
  policy = data.aws_iam_policy_document.cluster-autoscaler[0].json
}

resource "kubernetes_namespace" "chaos-mesh" {
  count = var.enable_chaos_mesh ? 1 : 0

  metadata {
    annotations = {
      name = "chaos-mesh"
    }

    name = "chaos-mesh"
  }
}

resource "helm_release" "chaos-mesh" {
  count     = var.enable_chaos_mesh ? 1 : 0
  name      = "chaos-mesh"
  namespace = kubernetes_namespace.chaos-mesh[0].metadata[0].name

  chart       = "${path.module}/../helm/chaos"
  max_history = 10
  wait        = false

  values = [
    jsonencode({
      # Only create the ingress if an ACM certificate exists
      ingress = {
        enable                   = length(aws_acm_certificate.ingress) > 0 ? true : false
        domain                   = "chaos.${local.domain}"
        acm_certificate          = length(aws_acm_certificate.ingress) > 0 ? aws_acm_certificate.ingress[0].arn : null
        loadBalancerSourceRanges = join(",", var.client_sources_ipv4)
        aws_tags                 = local.aws_tags
      }
      chaos-mesh = {
        chaosDaemon = {
          podSecurityPolicy = true
          # tolerate pod assignment on nodes in the validator nodegroup
          tolerations = jsondecode(module.validator.helm_values)["validator"]["tolerations"]
        }
      }
    })
  ]
}
