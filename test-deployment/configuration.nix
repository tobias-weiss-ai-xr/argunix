{
  modulesPath,
  pkgs,
  ...
}:

let
  fqdn = "argunix.nix-consulting.net";
  cacheUrl = "s3://test-cache?endpoint=nbg1.your-objectstorage.com&region=nbg1&addressing-style=virtual";
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
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIL7I8SFIFoKEBJEPEEUp14PuwA1Z+olKcL3OKlaxI//6 tfc@framejonge"
  ];

  services.argunix = {
    enable = true;
    listen = "127.0.0.1:8080";

    systems = [
      "x86_64-linux"
      "aarch64-linux"
    ];

    settings = {
      external_url = "https://${fqdn}";
      builder_enrollment = {
        listen = "[::]:45678";
        token_path = "/var/lib/argunix-credentials/builder-enrollment-token";
      };

      schedule = {
        build_concurrency = 6;
        build_timeout_seconds = 10 * 60 * 60;
      };

      binary_caches = [
        {
          push_url = cacheUrl;
          public_url = "https://nbg1.your-objectstorage.com/test-cache";
          public_key = "test-cache:vUfGsNg1GFZRW1wHSFsjcklY2fpzGkPntpdOoW3mhTA=";
          signing_key_path = "/var/lib/argunix-credentials/cache/test-cache-priv.key";
        }
      ];

      # External docker registry the `registry-push` effect copies built
      # dockerTools images to. Unlike the integration test's local
      # `registry:2`, the opencode registry requires a login: `auth_path`
      # holds one `user:password` line, read at push time and handed to
      # `skopeo --dest-creds`.
      #
      # `namespace = "{slug}"`: the effect substitutes each repo's slug,
      # so this one entry serves every opencode repo. On GitLab the
      # registry path is the project path — which is exactly the slug —
      # so an image lands at
      # `registry.opencode.de/<repo-slug>/<image>:<tag>`.
      registries = {
        opencode = {
          url = "registry.opencode.de";
          namespace = "{slug}";
          auth_path = "/var/lib/argunix-credentials/opencode-registry-creds";
        };
      };

      forges = {
        github = {
          kind = "github";
          web_url = "https://github.com";
          token_path = "/var/lib/argunix-credentials/gh-token";
          repos = {
            "applicative-systems/mkdocs-flake" = { };
            "applicative-systems/nixos-appliance-ota-update" = { };
            "applicative-systems/nixos-test-driver-manual" = { };
            "tfc/attoparsecpp".watched_branches = [ "master" ];
            "tfc/cmake_cpp_example".watched_branches = [ "master" ];
            "tfc/nixos-configs" = { };
            "tfc/pprintpp".watched_branches = [ "master" ];
          };
        };
        gitlab = {
          kind = "gitlab";
          web_url = "https://gitlab.com";
          token_path = "/var/lib/argunix-credentials/gl-token";
          repos = {
            "jonge/pprintpp" = {
              watched_branches = [ "master" ];
            };
          };
        };
        codeberg = {
          kind = "forgejo";
          web_url = "https://codeberg.org";
          token_path = "/var/lib/argunix-credentials/fj-token";
          repos = {
            "tfc/argunix" = { };
            "tfc/pprintpp" = {
              watched_branches = [ "master" ];
            };
            "tfc/tulonix" = { };
          };
        };
        opencode = {
          kind = "gitlab";
          web_url = "https://gitlab.opencode.de";
          token_path = "/var/lib/argunix-credentials/opencode-token";
          repos = {
            "oci-community/images/applicative-systems/example-build-and-attest" = {
              push_to_registries = [ "opencode" ];
            };
            "oci-community/images/applicative-systems/images" = {
              push_to_registries = [ "opencode" ];
            };
          };
        };
      };
    };
  };

  systemd.services.argunix.environment = {
    AWS_SHARED_CREDENTIALS_FILE = "/var/lib/argunix-credentials/s3-credentials";
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

  nix.settings = {
    substituters = [
      "https://cache.nixos.org"
      cacheUrl
    ];
    trusted-public-keys = [
      "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
      "test-cache:vUfGsNg1GFZRW1wHSFsjcklY2fpzGkPntpdOoW3mhTA="
    ];
  };
}
