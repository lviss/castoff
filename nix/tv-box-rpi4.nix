{ lib, config, nixos-raspberrypi, ... }:

# The Raspberry Pi 4 half of the appliance config: real hardware modules
# from `nixos-raspberrypi` (github:nvmd/nixos-raspberrypi) layered under
# `nix/tv-box.nix`'s target-independent kiosk config, the same way
# `nix/tv-box-x86_64.nix` layers generic placeholders under it for the VM
# target. `flake.nix` builds this with `nixos-raspberrypi.lib.nixosInstaller`,
# which adds that flake's own `sd-image`/`raspberrypi-installer` modules on
# top of this one -- that combination is what makes
# `packages.aarch64-linux.tv-box-rpi4-image` a single image that's both
# flashable installation media and a ready-to-boot system (the partition
# table auto-expands to fill the SD card on first boot; no separate
# nixos-anywhere/disko install step). `nixos-raspberrypi.lib.nixosInstaller`
# (and its siblings `nixosSystem`/`nixosSystemFull`) pass `nixos-raspberrypi`
# into every module's `specialArgs` automatically, per its README, so this
# module doesn't need `flake.nix` to do that by hand.
{
  imports = with nixos-raspberrypi.nixosModules; [
    raspberry-pi-4.base
    raspberry-pi-4.display-vc4
    raspberry-pi-4.bluetooth
  ];

  # `nix/tv-box.nix` sets this with `mkDefault "24.11"` for the x86_64
  # placeholder target; this config actually installs against
  # nixos-raspberrypi's own pinned nixpkgs (see `flake.nix`), so pin it to
  # whatever release that pin resolves to rather than inheriting an
  # unrelated default.
  system.stateVersion = config.system.nixos.release;

  networking.hostName = "castoff-rpi4";
}
