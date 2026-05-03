{
  modulesPath,
  pkgs,
  ...
}:

let
  fqdn = "medusa.nix-consulting.net";

  # Placeholder secrets so medusa.service starts cleanly on first boot.
  # Replace with real values (e.g. via deploy-rs / agenix / sops-nix) before
  # pointing GitHub webhooks at this host.
  placeholderWebhookSecret = pkgs.writeText "medusa-placeholder-webhook" "REPLACE-ME-webhook-secret";
  placeholderGithubToken = pkgs.writeText "medusa-placeholder-token" "REPLACE-ME-github-token";
in
{
  imports = [
    "${modulesPath}/profiles/qemu-guest.nix"
  ];

  disko.devices = import ./disko.nix "/dev/sda";

  nixpkgs.hostPlatform = "x86_64-linux";
  system.stateVersion = "25.11";
  time.timeZone = "UTC";

  boot.loader.grub = {
    devices = [ "/dev/sda" ];
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  networking.hostName = "medusa";
  networking.domain = "nix-consulting.net";

  networking.useDHCP = false;
  systemd.network.enable = true;
  systemd.network.networks."10-wan" = {
    matchConfig.Name = "enp1s0";
    networkConfig.DHCP = "ipv4";
    address = [ "2a01:4f8:c014:a8e::/64" ];
    routes = [ { Gateway = "fe80::1"; } ];
  };

  networking.firewall.allowedTCPPorts = [
    80
    443
  ];

  services.openssh.enable = true;

  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH6Z4dj1RU+44lXXW1Dw6TW4cLtV/4+qJRO7vFOmyC6C tfc@ai"
  ];

  services.medusa = {
    enable = true;
    listen = "127.0.0.1:8080";
    credentials = {
      gh-webhook = "${placeholderWebhookSecret}";
      gh-token = "${placeholderGithubToken}";
    };
    settings = {
      external_url = "https://${fqdn}";
      forges.gh = {
        kind = "github";
        api_url = "https://api.github.com";
        webhook_secret_path = "$CREDENTIALS_DIRECTORY/gh-webhook";
        token_path = "$CREDENTIALS_DIRECTORY/gh-token";
      };
      repos = [ ];
    };
  };

  security.acme = {
    acceptTerms = true;
    defaults.email = "service@applicative.systems";
  };

  services.nginx = {
    enable = true;
    recommendedProxySettings = true;
    recommendedTlsSettings = true;
    recommendedOptimisation = true;
    recommendedGzipSettings = true;

    virtualHosts.${fqdn} = {
      enableACME = true;
      forceSSL = true;
      locations."/" = {
        proxyPass = "http://127.0.0.1:8080";
        # GitHub webhooks can be up to 25 MB; bump from nginx's 1 MB default.
        extraConfig = ''
          client_max_body_size 32m;
          proxy_read_timeout 120s;
        '';
      };
    };
  };

  environment.systemPackages = with pkgs; [
    curl
    git
    jq
    sqlite
  ];
}
