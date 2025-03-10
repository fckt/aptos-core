provider "kubernetes" {
  host                   = aws_eks_cluster.aptos.endpoint
  cluster_ca_certificate = base64decode(aws_eks_cluster.aptos.certificate_authority.0.data)
  token                  = data.aws_eks_cluster_auth.aptos.token
}

resource "kubernetes_storage_class" "io1" {
  metadata {
    name = "io1"
  }
  storage_provisioner = "kubernetes.io/aws-ebs"
  volume_binding_mode = "WaitForFirstConsumer"
  parameters = {
    type      = "io1"
    iopsPerGB = "50"
  }
}

resource "null_resource" "delete-gp2" {
  provisioner "local-exec" {
    command = <<-EOT
      aws --region ${var.region} eks update-kubeconfig --name ${aws_eks_cluster.aptos.name} --kubeconfig ${local.kubeconfig} &&
      kubectl --kubeconfig ${local.kubeconfig} delete --ignore-not-found storageclass gp2
    EOT
  }

  depends_on = [kubernetes_storage_class.io1]
}


resource "kubernetes_storage_class" "gp2" {
  metadata {
    name = "gp2"
    annotations = {
      "storageclass.kubernetes.io/is-default-class" = true
    }
  }
  storage_provisioner = "kubernetes.io/aws-ebs"
  volume_binding_mode = "WaitForFirstConsumer"
  parameters = {
    type = "gp2"
  }

  depends_on = [null_resource.delete-gp2]
}

resource "kubernetes_role_binding" "psp-kube-system" {
  metadata {
    name      = "eks:podsecuritypolicy:privileged"
    namespace = "kube-system"
  }

  role_ref {
    api_group = "rbac.authorization.k8s.io"
    kind      = "ClusterRole"
    name      = "eks:podsecuritypolicy:privileged"
  }

  subject {
    api_group = "rbac.authorization.k8s.io"
    kind      = "Group"
    name      = "system:serviceaccounts:kube-system"
  }
}

locals {
  kubeconfig = "/tmp/kube.config.${md5(timestamp())}"
}

resource "null_resource" "delete-psp-authenticated" {
  provisioner "local-exec" {
    command = <<-EOT
      aws --region ${var.region} eks update-kubeconfig --name ${aws_eks_cluster.aptos.name} --kubeconfig ${local.kubeconfig} &&
      kubectl --kubeconfig ${local.kubeconfig} delete --ignore-not-found clusterrolebinding eks:podsecuritypolicy:authenticated
    EOT
  }

  depends_on = [kubernetes_role_binding.psp-kube-system]
}

provider "helm" {
  kubernetes {
    host                   = aws_eks_cluster.aptos.endpoint
    cluster_ca_certificate = base64decode(aws_eks_cluster.aptos.certificate_authority.0.data)
    token                  = data.aws_eks_cluster_auth.aptos.token
  }
}

resource "helm_release" "calico" {
  name        = "calico"
  namespace   = "kube-system"
  chart       = "${path.module}/aws-calico/"
  max_history = 10
}

locals {
  helm_values = jsonencode({
    numValidators = var.num_validators
    imageTag      = var.image_tag
    chain = {
      era        = var.era
      chain_id   = var.chain_id
      chain_name = var.chain_name
    }
    validator = {
      name = var.validator_name
      storage = {
        class = kubernetes_storage_class.gp2.metadata[0].name
      }
      nodeSelector = {
        "eks.amazonaws.com/nodegroup" = "validators"
      }
      tolerations = [{
        key    = "aptos.org/nodepool"
        value  = "validators"
        effect = "NoExecute"
      }]
      remoteLogAddress = var.enable_logger ? "${local.workspace_name}-log-aptos-logger:5044" : null
    }
    fullnode = {
      storage = {
        class = kubernetes_storage_class.gp2.metadata[0].name
      }
      nodeSelector = {
        "eks.amazonaws.com/nodegroup" = "validators"
      }
      tolerations = [{
        key    = "aptos.org/nodepool"
        value  = "validators"
        effect = "NoExecute"
      }]
    }
    haproxy = {
      nodeSelector = {
        "eks.amazonaws.com/nodegroup" = "utilities"
      }
    }
    service = {
      domain = local.domain
    }
  })
}

resource "helm_release" "validator" {
  count       = var.helm_enable_validator ? 1 : 0
  name        = local.workspace_name
  chart       = var.helm_chart != "" ? var.helm_chart : "${path.module}/../../helm/aptos-node"
  max_history = 5
  wait        = false

  values = [
    local.helm_values,
    var.helm_values_file != "" ? file(var.helm_values_file) : "{}",
    jsonencode(var.helm_values),
  ]

  set {
    name  = "timestamp"
    value = var.helm_force_update ? timestamp() : ""
  }
}

resource "helm_release" "logger" {
  count       = var.enable_logger ? 1 : 0
  name        = "${local.workspace_name}-log"
  chart       = "${path.module}/../../helm/logger"
  max_history = 5
  wait        = false

  values = [
    jsonencode({
      logger = {
        name = "aptos-logger"
      }
      chain = {
        name = var.chain_name
      }
      serviceAccount = {
        create = false
        # this name must match the serviceaccount created by the aptos-node helm chart
        name = "${local.workspace_name}-aptos-node-validator"
      }
    }),
    jsonencode(var.logger_helm_values),
  ]

  set {
    name  = "timestamp"
    value = timestamp()
  }
}

resource "helm_release" "monitoring" {
  count       = var.enable_monitoring ? 1 : 0
  name        = "${local.workspace_name}-mon"
  chart       = "${path.module}/../../helm/monitoring"
  max_history = 5
  wait        = false

  values = [
    jsonencode({
      chain = {
        name = var.chain_name
      }
      validator = {
        name = var.validator_name
      }
      monitoring = {
        prometheus = {
          storage = {
            class = kubernetes_storage_class.gp2.metadata[0].name
          }
        }
      }
    }),
    jsonencode(var.monitoring_helm_values),
  ]

  set {
    name  = "timestamp"
    value = timestamp()
  }
}

resource "kubernetes_cluster_role" "debug" {
  metadata {
    name = "debug"
  }

  rule {
    api_groups = [""]
    resources  = ["pods/portforward", "pods/exec"]
    verbs      = ["create"]
  }
}

resource "kubernetes_role_binding" "debuggers" {
  metadata {
    name = "debuggers"
  }

  role_ref {
    api_group = "rbac.authorization.k8s.io"
    kind      = "ClusterRole"
    name      = kubernetes_cluster_role.debug.metadata[0].name
  }

  subject {
    kind = "Group"
    name = "debuggers"
  }
}

resource "kubernetes_role_binding" "viewers" {
  metadata {
    name = "viewers"
  }

  role_ref {
    api_group = "rbac.authorization.k8s.io"
    kind      = "ClusterRole"
    name      = "view"
  }

  subject {
    kind = "Group"
    name = "viewers"
  }
  subject {
    kind = "Group"
    name = "debuggers"
  }
}

resource "kubernetes_config_map" "aws-auth" {
  metadata {
    name      = "aws-auth"
    namespace = "kube-system"
  }

  data = {
    mapRoles = yamlencode(concat(
      [{
        rolearn  = aws_iam_role.nodes.arn
        username = "system:node:{{EC2PrivateDNSName}}"
        groups   = ["system:bootstrappers", "system:nodes"]
      }],
      var.iam_path == "/" ? [] : [{
        # Workaround for https://github.com/kubernetes-sigs/aws-iam-authenticator/issues/268
        # The entry above is still needed otherwise EKS marks the node group as unhealthy
        rolearn  = replace(aws_iam_role.nodes.arn, "role${var.iam_path}", "role/")
        username = "system:node:{{EC2PrivateDNSName}}"
        groups   = ["system:bootstrappers", "system:nodes"]
      }],
      [for role in var.k8s_admin_roles : {
        rolearn  = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:role/${role}"
        username = "${role}:{{SessionName}}"
        groups   = ["system:masters"]
      }],
      [for role in var.k8s_viewer_roles : {
        rolearn  = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:role/${role}"
        username = "${role}:{{SessionName}}"
        groups   = ["viewers"]
      }],
      [for role in var.k8s_debugger_roles : {
        rolearn  = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:role/${role}"
        username = "${role}:{{SessionName}}"
        groups   = ["debuggers"]
      }],
    ))
    mapUsers = yamlencode(concat(
      [for user in var.k8s_admins : {
        userarn  = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:user/${user}"
        username = user
        groups   = ["system:masters"]
      }],
      [for user in var.k8s_viewers : {
        userarn  = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:user/${user}"
        username = user
        groups   = ["viewers"]
      }],
      [for user in var.k8s_debuggers : {
        userarn  = "arn:aws:iam::${data.aws_caller_identity.current.account_id}:user/${user}"
        username = user
        groups   = ["debuggers"]
      }],
    ))
  }
}
