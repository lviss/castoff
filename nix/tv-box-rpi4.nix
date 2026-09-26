{ config, lib, pkgs, nixos-raspberrypi, ... }:

# The Raspberry Pi 4 half of the appliance config: real hardware modules
# from `nixos-raspberrypi` (github:nvmd/nixos-raspberrypi) layered under
# `nix/tv-box.nix`'s target-independent kiosk config, the same way
# `nix/tv-box-x86_64.nix` layers generic placeholders under it for the VM
# target. `flake.nix` builds this with `nixos-raspberrypi.lib.nixosSystemFull`
# (which auto-injects `nixos-raspberrypi` into every module's `specialArgs`,
# per its README, so this module doesn't need `flake.nix` to do that by
# hand) -- this module imports nixos-raspberrypi's `sd-image` module itself,
# rather than going through that flake's own `nixosInstaller` helper, which
# also pulls in `raspberrypi-installer.nix` and, through it, nixpkgs'
# `profiles/installation-device.nix`: the profile for installation *media*,
# not a deployed appliance (it force-enables `documentation.*`, creates a
# passwordless "nixos" account and a passwordless root account, autologins
# "nixos" at the console, and sets `PermitRootLogin = mkDefault "yes"` --
# confirmed by reading that profile directly; see `flake.nix`'s comment on
# `tv-box-rpi4-system` for the full list). Importing `sd-image` directly here
# keeps the actual flashability -- `config.system.build.sdImage`, the
# partition table auto-expanding to fill the SD card on first boot, no
# separate nixos-anywhere/disko install step -- without any of that.
{
  imports = with nixos-raspberrypi.nixosModules; [
    raspberry-pi-4.base
    raspberry-pi-4.display-vc4
    raspberry-pi-4.bluetooth
    sd-image
  ];

  # `raspberrypi-installer.nix` (not imported here -- see above) also carried
  # this real, unrelated-to-"installer media" boot fix: RPi's initrd fails
  # partway through writing itself when `boot.swraid`'s auto-assembly probing
  # is active ("/initrd.image: incomplete write (-28 != ...)", subsequently
  # failing to boot at all), so it's disabled the same way here.
  boot.swraid.enable = lib.mkForce false;

  # `nix/tv-box.nix` sets this with `mkDefault "24.11"` for the x86_64
  # placeholder target; this config actually installs against
  # nixos-raspberrypi's own pinned nixpkgs (see `flake.nix`), so it needs its
  # own value rather than inheriting an unrelated default. Frozen to a
  # literal (the release `config.system.nixos.release` resolved to at the
  # time this was written) rather than tracking that expression live: the
  # `nixos-raspberrypi` input is unpinned, so a routine `nix flake update
  # nixos-raspberrypi` would otherwise silently advance stateVersion on an
  # already-deployed box.
  system.stateVersion = "26.05";

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

  # Boot-time diagnostic dump for the real-hardware Wi-Fi/display issues this
  # target has actually hit -- overwritten every boot at
  # `/var/log/castoff-debug.log` (real ext4, on the box's own storage, not the
  # FAT firmware partition -- SSH plus a normal `cat`/`less` is how this gets
  # read, no special tooling needed). `RemainAfterExit = true` plus a plain
  # oneshot lets `systemctl status` show it as a simple pass/fail rather than
  # a service that's expected to keep running. Ordered after the units it's
  # actually diagnosing (not `network-online.target`: that target -- and the
  # `NetworkManager-wait-online` service backing it -- may never activate at
  # all when Wi-Fi is exactly what's failing, which would delay or hide the
  # dump precisely when it's most needed); a short fixed sleep stands in for
  # "networking has had a chance to settle" instead.
  systemd.services.castoff-debug-dump = {
    description = "Dump castoff/networking/display diagnostics for real-hardware debugging";
    wantedBy = [ "multi-user.target" ];
    after = [ "NetworkManager-ensure-profiles.service" "cage-tty1.service" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    # `rfkill` is not its own nixpkgs package here and isn't guaranteed to be
    # one of `util-linux`'s built binaries either -- the script below guards
    # its call with `command -v` instead of depending on it directly.
    path = [ pkgs.networkmanager pkgs.iproute2 pkgs.util-linux pkgs.gnugrep pkgs.gawk ];
    script = ''
      out=/var/log/castoff-debug.log
      sleep 20

      {
        echo "=== castoff-debug-dump: $(date -Is) ==="

        echo; echo "--- /etc/castoff/wifi-credentials.env (PSK redacted) ---"
        if [ -r /etc/castoff/wifi-credentials.env ]; then
          awk -F= '
            /^WIFI_PSK=/ { print "WIFI_PSK=<redacted, " length($0)-length("WIFI_PSK=") " chars>"; next }
            { print }
          ' /etc/castoff/wifi-credentials.env
        else
          echo "(not present or not readable)"
        fi

        echo; echo "--- nmcli general status ---"
        nmcli general status || true
        echo; echo "--- nmcli device status ---"
        nmcli device status || true
        echo; echo "--- nmcli connection show ---"
        nmcli connection show || true

        echo; echo "--- systemctl status NetworkManager-ensure-profiles.service ---"
        systemctl status --no-pager NetworkManager-ensure-profiles.service || true
        echo; echo "--- journalctl -u NetworkManager-ensure-profiles.service ---"
        journalctl --no-pager -u NetworkManager-ensure-profiles.service || true

        echo; echo "--- systemctl status cage-tty1.service ---"
        systemctl status --no-pager cage-tty1.service || true
        echo; echo "--- journalctl -b -u cage-tty1.service ---"
        journalctl --no-pager -b -u cage-tty1.service || true

        echo; echo "--- journalctl -b -u castoff-daemon (castoff-daemon runs as cage-tty1's own client process, not its own unit -- this is expected to be empty; its own log lines are in the cage-tty1 journal above) ---"
        journalctl --no-pager -b -u castoff-daemon || true

        echo; echo "--- ip addr ---"
        ip addr || true

        echo; echo "--- rfkill list ---"
        if command -v rfkill >/dev/null 2>&1; then
          rfkill list || true
        else
          echo "(rfkill not available on this system)"
        fi

        echo; echo "--- dmesg | grep -iE 'wifi|brcm|firmware' ---"
        dmesg | grep -iE 'wifi|brcm|firmware' || true
      } > "$out" 2>&1
    '';
  };
}
