{ config, lib, nixos-raspberrypi, ... }:

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

  # Unattended Wi-Fi join on first boot. This Pi has no keyboard, mouse or
  # monitor attached (see README's Wi-Fi setup instructions), so there is no
  # way to type credentials in after flashing -- they have to already be in
  # place before the SD card is ever booted.
  #
  # `nix/tv-box.nix` already turns on NetworkManager for every target
  # (`networking.networkmanager.enable`); NetworkManager's own
  # `ensureProfiles` mechanism
  # (nixpkgs' `nixos/modules/services/networking/networkmanager.nix`) declares
  # a connection profile whose secret fields are `$VARIABLE` placeholders,
  # substituted (via `envsubst`) from a plain `KEY=value` file at service
  # start -- exactly the "read a secret from a file, not the Nix store" shape
  # this needs, and the pattern its own option documentation uses for this
  # exact case.
  networking.networkmanager.ensureProfiles = {
    environmentFiles = [ "/etc/castoff/wifi-credentials.env" ];
    profiles.castoff-wifi = {
      connection = {
        id = "castoff-wifi";
        type = "wifi";
      };
      wifi = {
        mode = "infrastructure";
        ssid = "$WIFI_SSID";
      };
      wifi-security = {
        key-mgmt = "wpa-psk";
        psk = "$WIFI_PSK";
      };
      ipv4.method = "auto";
      ipv6.method = "auto";
    };
  };

  # Bakes a template of that file directly into the *image's* root
  # filesystem at build time, so it already exists, pre-flash, at the exact
  # path NetworkManager reads on first boot. This deliberately does NOT go
  # through `environment.etc`: NixOS regenerates `/etc` from the Nix store on
  # *every* boot (not just on a `nixos-rebuild switch`), so a file declared
  # that way would silently clobber whatever the setup person wrote to the
  # card before the first boot ever ran -- the exact opposite of what's
  # needed here. `sdImage.populateRootCommands` (standard nixpkgs sd-image
  # option, reused by nixos-raspberrypi's own sd-image module, which already
  # assigns it once for `/boot/firmware` -- confirmed safe to append a second
  # definition with `lib.mkAfter` via a minimal `evalModules` check, since
  # neither assignment gives the option an explicit type) instead copies the
  # file straight into the image's `/etc` at build time, outside of and
  # invisible to the `/etc` regeneration that happens at boot.
  sdImage.populateRootCommands = lib.mkAfter ''
    install -D -m600 ${./wifi-credentials.env.example} \
      ./files/etc/castoff/wifi-credentials.env
  '';
}
