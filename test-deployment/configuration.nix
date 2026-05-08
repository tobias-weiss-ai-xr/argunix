{
  modulesPath,
  pkgs,
  ...
}:

let
  fqdn = "argunix.nix-consulting.net";
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

  networking.hostName = "argunix";
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

  services.argunix = {
    enable = true;
    listen = "127.0.0.1:8080";
    credentials = {
      gh-token = "/var/lib/argunix-credentials/gh-token";
      gl-token = "/var/lib/argunix-credentials/gl-token";
      fj-token = "/var/lib/argunix-credentials/fj-token";
      builder-enrollment-token = "/var/lib/argunix-credentials/builder-enrollment-token";
    };
    settings = {
      external_url = "https://${fqdn}";
      builder_enrollment = {
        listen = "[::]:45678";
        token_path = "$CREDENTIALS_DIRECTORY/builder-enrollment-token";
      };
      forges = {
        github = {
          kind = "github";
          web_url = "https://github.com";
          token_path = "$CREDENTIALS_DIRECTORY/gh-token";
          repos = {
            "tfc/attoparsecpp".watched_branches = [ "master" ];
            "tfc/cmake_cpp_example".watched_branches = [ "master" ];
            "tfc/pprintpp".watched_branches = [ "master" ];
            "applicative-systems/mkdocs-flake" = { };
            "applicative-systems/nixos-test-driver-manual" = { };
            "applicative-systems/nixos-appliance-ota-update" = { };
          };
        };
        gitlab = {
          kind = "gitlab";
          web_url = "https://gitlab.com";
          token_path = "$CREDENTIALS_DIRECTORY/gl-token";
          repos = {
            "jonge/pprintpp" = {
              watched_branches = [ "master" ];
            };
          };
        };
        codeberg = {
          kind = "forgejo";
          web_url = "https://codeberg.org";
          token_path = "$CREDENTIALS_DIRECTORY/fj-token";
          repos = {
            "tfc/pprintpp" = {
              watched_branches = [ "master" ];
            };
          };
        };
      };
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
