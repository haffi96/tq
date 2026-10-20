# One tq-relay VM in London: Container-Optimized OS running the relay image
# with host networking, a static IP on the Standard network tier, a Let's
# Encrypt certificate (DNS-01 via Cloudflare) and an unproxied A record.

locals {
  name = "tq-relay-1"
  apis = [
    "compute.googleapis.com",
    "iap.googleapis.com",
    "secretmanager.googleapis.com",
  ]
}

resource "google_project_service" "apis" {
  for_each           = toset(local.apis)
  service            = each.value
  disable_on_destroy = false
}

# --- network ----------------------------------------------------------------

resource "google_compute_network" "tq" {
  name                    = "tq"
  auto_create_subnetworks = false
  depends_on              = [google_project_service.apis]
}

resource "google_compute_subnetwork" "tq" {
  name          = "tq-${var.region}"
  network       = google_compute_network.tq.id
  region        = var.region
  ip_cidr_range = "10.10.0.0/24"
}

# Standard tier: traffic enters and leaves Google's network near London
# rather than at the edge closest to the peer. Cheaper egress, and for UK
# robots and operators the path is the same.
resource "google_compute_address" "relay" {
  name         = local.name
  region       = var.region
  network_tier = "STANDARD"
  depends_on   = [google_project_service.apis]
}

resource "google_compute_firewall" "relay" {
  name    = "tq-relay-https"
  network = google_compute_network.tq.id
  # TCP 443: HTTPS (/api/*, the bundled web app). UDP 443: WebTransport.
  allow {
    protocol = "tcp"
    ports    = ["443"]
  }
  allow {
    protocol = "udp"
    ports    = ["443"]
  }
  source_ranges = ["0.0.0.0/0"]
  target_tags   = ["tq-relay"]
}

# SSH only through IAP: `gcloud compute ssh tq-relay-1 --tunnel-through-iap`.
resource "google_compute_firewall" "iap_ssh" {
  name          = "tq-iap-ssh"
  network       = google_compute_network.tq.id
  source_ranges = ["35.235.240.0/20"]
  target_tags   = ["tq-relay"]
  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}

# --- secrets ----------------------------------------------------------------

# The token secret shared by the relay, publishers and token minting.
resource "random_password" "shared_secret" {
  length  = 48
  special = false
}

resource "google_secret_manager_secret" "shared_secret" {
  secret_id = "tq-shared-secret"
  replication {
    auto {}
  }
  depends_on = [google_project_service.apis]
}

resource "google_secret_manager_secret_version" "shared_secret" {
  secret      = google_secret_manager_secret.shared_secret.id
  secret_data = random_password.shared_secret.result
}

resource "google_secret_manager_secret" "cloudflare_token" {
  secret_id = "tq-cloudflare-dns-token"
  replication {
    auto {}
  }
  depends_on = [google_project_service.apis]
}

resource "google_secret_manager_secret_version" "cloudflare_token" {
  secret      = google_secret_manager_secret.cloudflare_token.id
  secret_data = var.cloudflare_api_token
}

# --- VM ---------------------------------------------------------------------

resource "google_service_account" "relay" {
  account_id   = "tq-relay"
  display_name = "tq relay VM"
}

resource "google_project_iam_member" "relay" {
  for_each = toset(["roles/logging.logWriter", "roles/monitoring.metricWriter"])
  project  = var.project_id
  role     = each.value
  member   = "serviceAccount:${google_service_account.relay.email}"
}

resource "google_secret_manager_secret_iam_member" "relay" {
  for_each = {
    shared     = google_secret_manager_secret.shared_secret.id
    cloudflare = google_secret_manager_secret.cloudflare_token.id
  }
  secret_id = each.value
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.relay.email}"
}

resource "google_compute_instance" "relay" {
  name                      = local.name
  machine_type              = var.machine_type
  zone                      = var.zone
  tags                      = ["tq-relay"]
  allow_stopping_for_update = true

  boot_disk {
    initialize_params {
      image = "cos-cloud/cos-stable"
      size  = 10
      type  = "pd-balanced"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.tq.id
    access_config {
      nat_ip       = google_compute_address.relay.address
      network_tier = "STANDARD"
    }
  }

  service_account {
    email  = google_service_account.relay.email
    scopes = ["cloud-platform"]
  }

  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }

  metadata = {
    # COS runs cloud-init on every boot; edits apply at the next reboot
    # (`gcloud compute instances reset tq-relay-1`).
    user-data = templatefile("${path.module}/cloud-init.yaml", {
      hostname          = var.relay_hostname
      cors_origins      = join(",", var.web_origins)
      shared_secret_id  = google_secret_manager_secret.shared_secret.secret_id
      cloudflare_secret = google_secret_manager_secret.cloudflare_token.secret_id
      acme_email        = var.acme_email
      acme_staging      = var.acme_staging
    })
    # Read by the service at each start, so an image change needs only a
    # restart of tq-relay, not a reboot.
    tq-relay-image            = var.relay_image
    google-logging-enabled    = "true"
    google-monitoring-enabled = "true"
    enable-oslogin            = "TRUE"
  }

  lifecycle {
    # cos-stable is a family; don't replace the VM when it moves on (COS
    # updates itself in place).
    ignore_changes = [boot_disk[0].initialize_params[0].image]
  }

  depends_on = [
    google_secret_manager_secret_iam_member.relay,
    google_secret_manager_secret_version.shared_secret,
    google_secret_manager_secret_version.cloudflare_token,
  ]
}

# --- DNS --------------------------------------------------------------------

data "cloudflare_zone" "this" {
  filter = {
    name = var.cloudflare_zone
  }
}

resource "cloudflare_dns_record" "relay" {
  zone_id = data.cloudflare_zone.this.id
  name    = var.relay_hostname
  type    = "A"
  content = google_compute_address.relay.address
  ttl     = 300
  # Must stay unproxied: the Cloudflare proxy does not carry QUIC/UDP to
  # arbitrary origins, and the relay terminates TLS itself.
  proxied = false
  comment = "tq relay on GCP (infra/gcp, terraform)"
}
