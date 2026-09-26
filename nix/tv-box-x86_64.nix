{ lib, ... }:

# The x86_64-linux half of the appliance config: generic placeholders so
# `nixosConfigurations.tv-box` (`nix/tv-box.nix` plus this module) is
# bootable and `nix build`-able on its own (`nix flake check`,
# `nixos-rebuild build-vm`, or installing to any disk labeled "nixos").
# There is no real x86_64 appliance hardware to target yet, so this is as
# concrete as the config gets -- unlike `nix/tv-box-rpi4.nix`, which layers
# onto real Raspberry Pi 4 hardware modules from `nixos-raspberrypi`.
{
  fileSystems."/" = lib.mkDefault {
    device = "/dev/disk/by-label/nixos";
    fsType = "ext4";
  };
  boot.loader.systemd-boot.enable = lib.mkDefault true;
  boot.loader.efi.canTouchEfiVariables = lib.mkDefault true;
}
