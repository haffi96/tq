# tq-relay on GCP

One relay VM in London (`europe-west2`), with the web app on Cloudflare.

```text
browser ── https://tq.haffi.dev ──────────────► Cloudflare (static web app, web/wrangler.jsonc)
   │
   ├── https://tq-relay-1.haffi.dev/api/*  (TCP 443)  ┐
   └── WebTransport tq-relay-1.haffi.dev   (UDP 443)  ├► GCE VM: COS + docker, relay image
vehicle ── tq-publisher ── QUIC            (UDP 443)  ┘   Let's Encrypt cert (DNS-01, Cloudflare)
```

What Terraform creates:
- a VPC and subnet
- a static IPv4 on the Standard network tier
- firewall rules: TCP and UDP 443 from anywhere, SSH from IAP only
- an `e2-standard-2` VM on Container-Optimized OS, set up by `cloud-init.yaml`
- Secret Manager secrets for the token secret (generated) and the Cloudflare token
- a service account that can read only those two secrets
- the `tq-relay-1.haffi.dev` A record in Cloudflare, not proxied: Cloudflare's proxy cannot carry QUIC

The relay serves both protocols on port 443: QUIC over UDP and HTTPS over TCP.
It also serves its bundled web app, so `https://tq-relay-1.haffi.dev/?av=…`
works as well as `https://tq.haffi.dev/?av=…`.

## Prerequisites

- `gcloud` and `terraform` (1.6 or newer)
- A GCP project with billing enabled. `gcloud auth application-default login`
  gives Terraform your credentials.
- A Cloudflare API token with **Zone → DNS → Edit** on `haffi.dev`. Terraform
  uses it for the A record. certbot on the VM uses it for DNS-01.
- The relay image: `ghcr.io/haffi96/tq-relay:0.2.0` (or newer). It needs
  `--http-tls` and `--cors-origins`.

## Deploy

```sh
cd infra/gcp
cp terraform.tfvars.example terraform.tfvars   # project_id, cloudflare_api_token
terraform init
terraform apply
```

The first boot takes 2–3 minutes: pull certbot, issue the certificate, pull
the relay. Then:

```sh
curl https://tq-relay-1.haffi.dev/api/info
```

To watch the first boot:

```sh
$(terraform output -raw ssh)
sudo journalctl -u tq-secrets -u tq-cert -u tq-relay -f
```

Logs also go to Cloud Logging.

While you are testing, set `acme_staging = true`. Let's Encrypt allows only 5
certificates per week for the same name, and recreating the VM requests a new one.

## Tokens

There is no `--dev`, so nobody can mint tokens over HTTP. Mint them with the
same secret:

```sh
export TQ_SHARED_SECRET=$(terraform output -raw shared_secret)
cargo run -p tq-relay -- token --role operator --av robot-01 --ttl-hours 12
```

Then open `https://tq.haffi.dev/?av=robot-01&role=operator&token=…`.

Publisher config:

```json
"relay_url": "https://tq-relay-1.haffi.dev:443",
"relay_trust": { "mode": "system_roots" },
"shared_secret": "<terraform output -raw shared_secret>"
```

## Operating

| Task | How |
| --- | --- |
| Upgrade the relay | Set `relay_image`, `terraform apply`, then on the VM `sudo systemctl restart tq-relay` |
| Change cloud-init | `terraform apply`, then `gcloud compute instances reset tq-relay-1` (cloud-init runs on every boot) |
| Certificate renewal | A daily timer runs certbot. If the certificate changed, it restarts the relay. |
| Change machine type | `machine_type`, `terraform apply` (stops and starts the VM) |

Restarting the relay, which includes a certificate renewal (about every 60
days), drops every session. The vehicle's dead-man switch trips, clients
reconnect, and the operator has to claim again.

## Cost (London, approximate)

- `e2-standard-2`: about $57/month. `e2-medium` is about half that, but it has
  shared, bursting vCPUs.
- Static IP in use, 10 GB disk, Secret Manager: a few dollars.
- **Egress is what grows:** about $0.085/GB on the Standard tier, with
  200 GB/month free. One viewer of 4 cameras at 2 Mbps uses about 3.6 GB/hour.
