# Single-disk simple GPT layout: legacy BIOS-boot stub + ESP + ext4 root.
# Same shape as the reference at
# https://github.com/tfc/nixos-anywhere-example/blob/main/single-gpt-disk-fullsize-ext4.nix
diskDevice:

{
  disk.${diskDevice} = {
    device = diskDevice;
    type = "disk";
    content = {
      type = "gpt";
      partitions = {
        boot = {
          priority = 0;
          size = "1M";
          type = "EF02";
        };
        ESP = {
          priority = 1;
          size = "512M";
          type = "EF00";
          content = {
            type = "filesystem";
            format = "vfat";
            mountpoint = "/boot";
          };
        };
        root = {
          priority = 3;
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint = "/";
          };
        };
      };
    };
  };
}
