variable "project_id" {
  description = "GCP project to deploy into (billing enabled)."
  type        = string
}

variable "region" {
  description = "London."
  type        = string
  default     = "europe-west2"
}

variable "zone" {
  type    = string
  default = "europe-west2-b"
}

variable "machine_type" {
  description = <<-EOT
    e2-standard-2: 2 dedicated vCPUs, 8 GB, up to 4 Gbps egress, about $57/month
    in London. e2-medium (shared core, bursts) is about half that, but a
    throttled vCPU shows up as video and teleop jitter.
  EOT
  type        = string
  default     = "e2-standard-2"
}

variable "relay_image" {
  description = "Relay container image. Read at each service start: change it, apply, then `sudo systemctl restart tq-relay` on the VM."
  type        = string
  default     = "ghcr.io/haffi96/tq-relay:0.2.0"
}

variable "relay_hostname" {
  description = "DNS name of the relay (A record, not proxied: Cloudflare cannot proxy QUIC)."
  type        = string
  default     = "tq-relay-1.haffi.dev"
}

variable "web_origins" {
  description = "Origins allowed to call the relay's /api/* (the Cloudflare-hosted web app)."
  type        = list(string)
  default     = ["https://tq.haffi.dev"]
}

variable "cloudflare_zone" {
  type    = string
  default = "haffi.dev"
}

variable "cloudflare_api_token" {
  description = "Token with Zone > Zone > Read and Zone > DNS > Edit on the zone. Terraform looks the zone up by name and creates the A record; certbot on the VM uses it for DNS-01."
  type        = string
  sensitive   = true
}

variable "acme_email" {
  description = "Let's Encrypt account email for expiry notices. Empty: register without one."
  type        = string
  default     = ""
}

variable "acme_staging" {
  description = "Use Let's Encrypt staging (untrusted certificates, generous rate limits) while testing."
  type        = bool
  default     = false
}
