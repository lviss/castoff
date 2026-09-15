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

  # Everything below is the *VM-only* half of this module. NixOS's own
  # `virtualisation.vmVariant` (nixos/modules/virtualisation/build-vm.nix) is
  # the supported hook for "configuration that only applies to the VM built by
  # `nixos-rebuild build-vm`": the flake's `tv-box-vm` package is
  # `config.system.build.vm`, which is `virtualisation.vmVariant`, so both
  # documented ways of booting the VM build this same configuration -- and
  # the appliance itself never even evaluates it. Nothing here changes what
  # the real box does.
  #
  # See README's "NixOS VM, no TV hardware required" for what the VM looks
  # like from outside, and AGENTS.md for the measurements behind each knob.
  virtualisation.vmVariant = {
    virtualisation = {
      graphics = true;
      memorySize = 2048;

      # QEMU's default x86 VGA adapter is `std` (bochs-drm), which has no
      # render node at all: `EGL device` enumeration finds nothing, so neither
      # cage nor mpv can create a GL context, and nothing can be drawn. The
      # virtio GPU gives the guest a `/dev/dri/renderD128` whose EGL works
      # through Mesa's software path (`kms_swrast`, llvmpipe) -- no host GPU or
      # host GL needed, since this is plain 2D virtio-gpu, not virgl. That is
      # what lets the kiosk draw at all in the VM.
      qemu.options = [
        "-vga"
        "virtio"
      ];

      # FCast's default port, forwarded from the host so a sender running on
      # the host -- the Android app, a test script, `socat` -- reaches the
      # daemon in the VM with no manual qemu wiring. SLiRP's host side binds
      # 0.0.0.0 (SLiRP only does IPv4), i.e. every host interface, exactly
      # like the appliance's own listener; see README for the address to send
      # to and how to restrict it to loopback.
      forwardPorts = [
        {
          from = "host";
          host.port = 46899;
          guest.port = 46899;
        }
      ];
    };

    # Plymouth's shutdown does not complete in this VM under the qemu-vm
    # module's default kernel command line: the boot stops at
    # `plymouth-quit.service` ("Terminate Plymouth Boot Screen"), and since
    # `cage-tty1.service` is ordered `After=plymouth-quit.service` and
    # `plymouth-quit-wait.service` ("Hold until boot process finishes up")
    # holds up `multi-user.target`, the kiosk compositor is *never started at
    # all* -- the screen just sits on a cleared VT with a blinking cursor,
    # which is exactly the blank-screen report this VM-only block exists to
    # fix. (The same image boots to `graphical.target` in ~11s and starts cage
    # with `plymouth.enable=0` on the kernel command line; with plymouth
    # enabled, neither `plymouth-quit` nor `plymouth-quit-wait` ever reaches a
    # `Finished` line, and changing which console `/dev/console` points at
    # changes that -- so what hangs is plymouth's own console handover.) A
    # boot splash buys an appliance-less VM nothing, so the VM doesn't run one.
    boot.plymouth.enable = lib.mkForce false;

    # Neither cage nor mpv can get a *hardware* GL context here, so both are
    # told to accept Mesa's software (llvmpipe) one: wlroots refuses software
    # EGL unless explicitly allowed, and its alternative -- the pixman
    # renderer -- advertises no dma-buf interface, which leaves mpv with no
    # surface it can present into. With this, cage runs its real DRM backend
    # and GLES2 renderer and mpv runs its real `vo=gpu`, just drawn by
    # llvmpipe. Hardware-accelerated rendering is the one capability the VM
    # genuinely cannot have; it degrades to software rendering, not to a
    # blank screen.
    services.cage.environment.WLR_RENDERER_ALLOW_SOFTWARE = "1";

    # mpv's automatic GPU-context probing tries its Vulkan and then X11
    # contexts before Wayland, and merely *connecting* to the X display cage
    # advertises is enough to make wlroots start its lazily-spawned Xwayland.
    # Xwayland cannot bring up a screen without a GPU (`Refusing to try glamor
    # on llvmpipe` -> `Fatal server error: Couldn't add screen` -> abort), and
    # wlroots asserts while tearing down its dead surface
    # (`xwayland/xwm.c:592`), killing cage and the whole kiosk session -- the
    # second blank-screen failure this VM-only block fixes. Pinning mpv to the
    # `wayland` context keeps it off X11 entirely (so Xwayland never starts)
    # and on the Wayland/EGL path it already uses on real hardware, where
    # auto-detection reaches it first.
    services.cage.environment.CASTOFF_MPV_GPU_CONTEXT = "wayland";

    # The same X11 wakeup, one layer down: `hwdec=auto-safe` (the appliance
    # default) probes VDPAU, whose backend lookup opens an X display -- so on
    # the VM merely *playing* something started the same doomed Xwayland, even
    # with the context pinned above. `hwdec=no` is not a loss here: this guest
    # has no hardware decoder for any hwdec to find. Both knobs are
    # troubleshooting knobs in `daemon/src/player.rs`; the appliance leaves
    # both unset (`gpu-context=auto`, `hwdec=auto-safe`).
    services.cage.environment.CASTOFF_MPV_HWDEC = "no";

    # A console that survives a broken kiosk session, so a blank screen comes
    # with a shell to inspect instead of a dead rectangle. The qemu-vm module
    # already puts `console=ttyS0` on the kernel command line (systemd's own
    # getty generator then runs `serial-getty@ttyS0`), so this only adds
    # autologin. The QEMU *window* shows the VGA console; the serial console
    # is the window's "Serial 0" tab (Ctrl-Alt-3) -- see README.
    systemd.services."serial-getty@ttyS0" = {
      # `serial-getty@ttyS0` is an instance of systemd's `serial-getty@`
      # template, and systemd ignores unit *files* named after an instance --
      # a drop-in is the only thing that reaches it (this is what
      # `overrideStrategy = "asDropin"` is for; a plain
      # `systemd.services."serial-getty@ttyS0".serviceConfig` writes a file
      # systemd never reads and silently leaves the login prompt in place).
      overrideStrategy = "asDropin";
      serviceConfig.ExecStart = lib.mkForce [
        ""
        "${pkgs.util-linux}/bin/agetty --autologin root --login-program ${pkgs.shadow}/bin/login %I --keep-baud $TERM"
      ];
    };
  };
}
