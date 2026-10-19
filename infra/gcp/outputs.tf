output "relay_ip" {
  value = google_compute_address.relay.address
}

output "relay_url" {
  description = "Relay HTTPS base (also serves the bundled web app)."
  value       = "https://${var.relay_hostname}"
}

output "shared_secret" {
  description = "Token secret: publisher config, `tq-relay token`. `terraform output -raw shared_secret`."
  value       = random_password.shared_secret.result
  sensitive   = true
}

output "ssh" {
  value = "gcloud compute ssh ${google_compute_instance.relay.name} --zone ${var.zone} --project ${var.project_id} --tunnel-through-iap"
}
