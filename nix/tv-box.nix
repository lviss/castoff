{ lib, pkgs, config, castoffDaemon, ... }:

# NixOS module for the castoff TV-box appliance: boots straight into Cage
# (a wlroots-based single-app Wayland kiosk compositor, no desktop
# environment or display manager) running the castoff daemon as its client.
# The daemon embeds mpv to play media and spawns Chromium as a second client
# of the same Cage session to display web pages; Cage stacks the newer
# client on top, so a cast page covers mpv and taking it down reveals mpv's
# idle clock again.
#
# Adapted from the kiosk pattern in matthewbauer/nixos-kiosk and
# matthewbauer/nixiosk (both: NixOS + Cage, one dedicated "kiosk" user,
# avahi for LAN discovery, sshd for remote access, unneeded desktop
# services stripped out) but using nixpkgs' own `services.cage` module
# instead of hand-rolled `systemd.services."cage@"` units, since that
# module has since landed upstream.
{
  imports = [ ];

  services.cage = {
    enable = true;
    user = "kiosk";
    program = "${castoffDaemon}/bin/castoff-daemon";
  };

  users.users.kiosk = {
    isNormalUser = true;
    description = "castoff kiosk session user";
    extraGroups = [ "video" "audio" "input" ];
  };

  # Audio for mpv playback.
  security.rtkit.enable = true;
  services.pipewire = {
    enable = true;
    alsa.enable = true;
    pulse.enable = true;
  };

  # FCast control API.
  networking.firewall.allowedTCPPorts = [ 46899 ];

  # LAN discovery, matching the nixiosk/nixos-kiosk precedent of advertising
  # the box over avahi/mDNS so a sender app can find it without static IPs.
  services.avahi = {
    enable = true;
    nssmdns4 = true;
    publish = {
      enable = true;
      userServices = true;
      addresses = true;
    };
  };

  # Boat power, not mains: this box should sit idle drawing as little power
  # as possible until something is cast to it, so anything that spins the
  # disk, polls hardware, or exists only for desktop-workstation ergonomics
  # is trimmed, matching nixos-kiosk's "shrink closure" approach.
  services.udisks2.enable = false;
  documentation.enable = false;
  documentation.nixos.enable = false;
  powerManagement.enable = true;
  programs.command-not-found.enable = false;
  boot.plymouth.enable = true;
  systemd.services."getty@tty1".enable = false;

  networking.networkmanager.enable = lib.mkDefault true;

  time.timeZone = lib.mkDefault "UTC";
  system.stateVersion = lib.mkDefault "24.11";

  # Generic x86_64-linux placeholders so this configuration is bootable and
  # `nix build`-able on its own (`nix flake check`, `nixos-rebuild build-vm`,
  # or installing to any disk labeled "nixos"). Flashing a real appliance
  # image to specific hardware -- disko partitioning, nixos-generators, etc.
  # -- is follow-up work; see README roadmap.
  fileSystems."/" = lib.mkDefault {
    device = "/dev/disk/by-label/nixos";
    fsType = "ext4";
  };
  boot.loader.systemd-boot.enable = lib.mkDefault true;
  boot.loader.efi.canTouchEfiVariables = lib.mkDefault true;
}
