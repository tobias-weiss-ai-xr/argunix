# argunix Integration with openDesk Edu

<!--
SPDX-FileCopyrightText: 2026 openDesk Edu Contributors
SPDX-License-Identifier: Apache-2.0
-->

This document describes the integration of **argunix** into the **openDesk Edu** ecosystem as a Nix-native CI build tool.

## Overview

[argunix](https://codeberg.org/tfc/argunix) has been integrated into the openDesk Edu ecosystem to provide automated Nix flake evaluation and build automation. This integration enables:

- **Automatic flake evaluation** on push and PR events
- **Declarative configuration** via Nix flakes
- **Nix-native CI** for all openDesk services
- **Binary cache integration** with automatic SBOM generation
- **Multi-forge support** (GitHub, GitLab, Forgejo/Codeberg)

## Repository Changes

### 1. argunix (This Repository)
- **Status**: Pushed to GitHub as `tobias-weiss-ai-xr/argunix`
- **Source**: Forked from Codeberg upstream
- **Purpose**: Main argunix CI system

### 2. opendesk-meta
- **Repository**: https://github.com/tobias-weiss-ai-xr/opendesk-meta
- **Changes**:
  - Added complete **argunix Helm chart** at `helmfile/charts/argunix/`
  - Added argunix to **component matrix** in README.md
  - Added argunix to **tech stack** table
  - Added documentation at `docs/ci-cd/argunix-integration.md`

### 3. opendesk-edu
- **Repository**: https://github.com/tobias-weiss-ai-xr/opendesk-edu
- **Changes**:
  - Added **argunix app configuration** at `helmfile/apps/edu/argunix/`
  - Updated **ce-overrides.yaml.gotmpl** to include argunix (enabled by default)
  - Created **helmfile.yaml.gotmpl** for argunix deployment
  - Created **values.yaml.gotmpl** with default configuration

### 4. opendesk-nix
- **Repository**: https://github.com/tobias-weiss-ai-xr/opendesk-nix
- **Changes**:
  - Added **argunix service module** at `platform/nix/services/argunix.nix`
  - Added argunix to **services catalog** in `platform/nix/nixos/services.nix`
  - Added **Dockerfile** for argunix-builder at `docker/argunix-builder/`
  - Added Nix configuration files for builder

## Architecture

### argunix Components in Kubernetes

```
┌─────────────────────────────────────────────────────────────┐
│                     Kubernetes Cluster                         │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  ┌─────────────────┐     ┌─────────────────┐                   │
│  │  argunix         │     │  argunix-nats   │                   │
│  │  Coordinator     │◄────┤  (NATS Server) │                   │
│  │  StatefulSet    │     │  StatefulSet    │                   │
│  └────────┬────────┘     └─────────────────┘                   │
│           │                                                         │
│           │  ┌─────────────────┐                                   │
│           │  │  argunix         │                                   │
│           └──►│  Builder         │ (External, connects via SSH)    │
│              │  (Pod/Node)      │                                   │
│              └─────────────────┘                                   │
│                                                               │
│  ┌─────────────────┐     ┌─────────────────┐                   │
│  │  Ingress         │────►│  argunix         │                   │
│  │  (TLS Term.)     │     │  Service        │                   │
│  └─────────────────┘     └────────┬────────┘                   │
│                                    │                            │
│                                    ▼                            │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │              Persistent Volume (SQLite DB)               │ │
│  └─────────────────────────────────────────────────────────┘ │
│                                                               │
└─────────────────────────────────────────────────────────────┘
```

### Integration Points

1. **openDesk Deployment**
   - argunix deployed via Helm chart
   - coordinated through helmfile
   - integrated with existing monitoring (Prometheus)

2. **openDesk Nix**
   - argunix service definition in Nix
   - builder Docker images for containerized builds
   - Nix module for configuration

3. **Binary Cache**
   - Automatic cache push on successful builds
   - SBOM generation support
   - Multi-registry support (S3, GHCR, GitLab)

## Deployment

### Deploy argunix via Helmfile

```bash
cd opendesk-meta/opendesk-edu
helmfile -e edu apply
```

### Enable argunix

In your environment configuration (`helmfile/environments/edu/ce-overrides.yaml.gotmpl`):

```yaml
apps:
  argunix:
    enabled: true
```

### Configure Forges

Edit the values in `opendesk-edu/helmfile/apps/edu/argunix/values.yaml.gotmpl`:

```yaml
argunix:
  config:
    forges:
      github:
        kind: github
        web_url: https://github.com
        token_path: /var/lib/argunix-credentials/github-token
        repos:
          "opendesk-edu/opendesk-edu": {}
          "opendesk-edu/opendesk-nix": {}
      gitlab:
        kind: gitlab
        web_url: https://gitlab.opencode.de
        token_path: /var/lib/argunix-credentials/gitlab-token
        repos:
          "opendesk-edu/opendesk-meta": {}
```

## Configuration Reference

### Helm Chart Values

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `enabled` | bool | true | Enable argunix deployment |
| `image.repository` | string | "ghcr.io/tobias-weiss-ai-xr/argunix" | Image repository |
| `image.tag` | string | "main" | Image tag |
| `replicas` | int | 1 | Number of coordinator replicas |
| `external_url` | string | "https://ci.opendesk.example.com" | External URL |
| `database.type` | string | "sqlite" | Database type (sqlite or postgresql) |
| `database.path` | string | "/var/lib/argunix/argunix.sqlite" | SQLite path |
| `nats.enabled` | bool | true | Enable NATS for event streaming |
| `builderEnrollment.enabled` | bool | true | Enable builder enrollment |
| `ingress.enabled` | bool | true | Enable ingress |
| `serviceMonitor.enabled` | bool | false | Enable Prometheus ServiceMonitor |

### Nix Module Options

Available in `platform/nix/services/argunix.nix`:

```nix
services.argunix = {
  enable = true;
  version = "0.0.1-dev";
  externalUrl = "https://ci.opendesk.example.com";
  listen = "0.0.0.0:8080";
  database.type = "sqlite";
  database.path = "/var/lib/argunix/argunix.sqlite";
  nats.url = "nats://localhost:4222";
  builderEnrollment.enabled = true;
  builderEnrollment.listen = "0.0.0.0:45678";
  forges = {};  # Forge configurations
  binaryCaches = [];  # Binary cache configurations
  nix.store = "/nix";
  nix.trusted_users = [ "root" "argunix" "argunix-builder" ];
};
```

## Usage

### As a CI System

argunix automatically:
1. Receives webhooks from configured forges
2. Evaluates Nix flakes from the repository
3. Discovers all `packages.<system>`, `checks.<system>`, `devShells.<system>`, and `nixosConfigurations`
4. Schedules and executes builds
5. Pushes successful builds to binary cache
6. Posts status updates back to the forge

### As a Build Tool

Use argunix to build individual flakes:

```bash
kubectl exec -it deploy/argunix -- argunixctl eval https://github.com/opendesk-edu/opendesk-nix
kubectl exec -it deploy/argunix -- argunixctl eval https://github.com/opendesk-edu/opendesk-nix --ref main
```

### With Builders

1. **Enroll a builder**:
   ```bash
   echo "ENROLLMENT_TOKEN" > /var/lib/argunix-builder/enrollment-token
   chmod 600 /var/lib/argunix-builder/enrollment-token
   ```

2. **Configure builder service** (NixOS):
   ```nix
   services.argunix-builder = {
     enable = true;
     argunixHost = "ci.opendesk.example.com";
     argunixPort = 45678;
     enrollmentTokenFile = "/var/lib/argunix-builder/enrollment-token";
   };
   ```

3. **Start builder**:
   ```bash
   systemctl start argunix-builder
   ```

## Monitoring

argunix exposes Prometheus metrics at `/metrics`:

```yaml
# Enable monitoring
argunix:
  serviceMonitor:
    enabled: true
    namespace: monitoring
    labels:
      release: kube-prometheus-stack
```

### Key Metrics

- `argunix_evals_total` - Total flake evaluations
- `argunix_evals_duration_seconds` - Evaluation duration
- `argunix_jobs_total` - Total build jobs
- `argunix_jobs_duration_seconds` - Job duration
- `argunix_queue_length` - Current queue length
- `argunix_builders_connected` - Connected builders

## Security

### Authentication

- **Forge tokens**: Required for each forge (GitHub, GitLab, etc.)
- **Builder tokens**: Shared secret for builder enrollment
- **Nix daemon**: Builders run as trusted users in Nix daemon

### Permissions

**GitHub Token Scopes**:
- `Contents: read`
- `Commit statuses: read/write`
- `Webhooks: read/write`
- `Pull requests: read`

**GitLab Token Scopes**:
- `read_repository`
- `write_repository` (for webhooks and statuses)

### Network Security

- **Ingress**: HTTPS only with TLS termination
- **Builder connection**: SSH from builders to coordinator
- **NATS**: Internal communication only (ClusterIP)
- **Pod Security**: Non-root, read-only root filesystem, capability dropping

## Development

### Local Development

1. Clone repositories:
   ```bash
   git clone https://github.com/tobias-weiss-ai-xr/argunix
   git clone https://github.com/tobias-weiss-ai-xr/opendesk-meta
   git clone https://github.com/tobias-weiss-ai-xr/opendesk-nix
   ```

2. Build argunix from source:
   ```bash
   cd argunix
   nix build
   ```

3. Build Docker images:
   ```bash
   cd opendesk-nix
   docker build -t argunix -f docker/argunix/Dockerfile .
   docker build -t argunix-builder -f docker/argunix-builder/Dockerfile .
   ```

### Testing

1. Deploy argunix in test environment:
   ```bash
   cd opendesk-meta/opendesk-edu
   helmfile -e edu-test apply
   ```

2. Create test repository with Nix flake
3. Configure forge webhook to point to argunix
4. Push changes to trigger build

## Troubleshooting

### Common Issues

1. **Webhook not triggering**:
   - Check forge token permissions
   - Verify webhook URL is correct
   - Check argunix logs: `kubectl logs -l app.kubernetes.io/name=argunix -c argunix`

2. **Builds not starting**:
   - Check builder enrollment: `kubectl exec -it deploy/argunix -- argunixctl builders list`
   - Verify builder token is correct
   - Check builder is online

3. **Nix build failures**:
   - Check Nix configuration: `kubectl get configmap argunix-config -o yaml`
   - Verify trusted users: `kubectl exec -it deploy/argunix -- cat /etc/nix/nix.conf`
   - Check Nix daemon is running

4. **Database errors**:
   - Check persistent volume: `kubectl get pvc -l app.kubernetes.io/name=argunix`
   - Verify database type configuration
   - Check database path permissions

### Debug Mode

Enable debug logging in values:

```yaml
argunix:
  config:
    log_level: "debug"
```

Or in Nix configuration:

```nix
services.argunix.logLevel = "debug";
```

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Submit a pull request

### Testing Changes

1. Test locally with minikube:
   ```bash
   minikube start
   helmfile -e test apply
   ```

2. Test with real forge (GitHub/GitLab):
   - Create test repository
   - Configure webhook
   - Push changes

3. Test with builders:
   - Deploy builder in NixOS VM
   - Enroll with coordinator
   - Verify builds execute on builder

## Resources

- [argunix Documentation](https://codeberg.org/tfc/argunix/src/branch/main/docs)
- [argunix Source Code](https://codeberg.org/tfc/argunix)
- [openDesk Edu](https://github.com/opendesk-edu/opendesk-edu)
- [openDesk Nix](https://github.com/opendesk-edu/opendesk-nix)
- [openDesk Meta](https://github.com/opendesk-edu/opendesk-meta)

## License

This integration is licensed under **Apache 2.0** license.

## Copyright

Copyright (C) 2026 openDesk Edu Contributors
