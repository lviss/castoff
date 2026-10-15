{
  description = "castoff: an open-source, appliance-like Chromecast/Roku alternative";

  nixConfig = {
    # nixos-raspberrypi's binary cache. Without it, `nix build
    # .#tv-box-rpi4-image` recompiles the Raspberry Pi vendor kernel from
    # source, which is slow; the flake's own README recommends this exact
    # cache. It applies only to derivations nixos-raspberrypi actually
    # provides, so it has no effect on the x86_64-linux outputs.
    extra-substituters = [ "https://nixos-raspberrypi.cachix.org" ];
    extra-trusted-public-keys = [
      "nixos-raspberrypi.cachix.org-1:4iMO9LXa8BqhU+Rpg6LQKiGa2lsNh/j2oiYLNOQ5sPI="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    # Second nixpkgs, used for exactly one package: `yt-dlp`. See `yt-dlp`
    # below for why the stable pin isn't good enough. Nothing else here is
    # imported from this input.
    nixpkgs-unstable.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Raspberry Pi 4 hardware enablement (kernel, firmware, vc4-kms-v3d
    # display, Bluetooth, the sd-image/installer image builder) for
    # `nixosConfigurations.tv-box-rpi4` -- see nix/tv-box-rpi4.nix. Not
    # `nixpkgs.follows`-ed: its hardware modules are validated against its
    # own pinned nixpkgs, and that pin only affects the aarch64-linux
    # outputs it feeds, not this flake's x86_64-linux ones.
    nixos-raspberrypi.url = "github:nvmd/nixos-raspberrypi";
  };

  outputs = { self, nixpkgs, nixpkgs-unstable, nixos-raspberrypi }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forEachSystem = nixpkgs.lib.genAttrs systems;

      # yt-dlp is deliberately the one package taken from nixos-unstable
      # rather than the nixos-25.11 pin above. nixos-25.11 ships
      # yt-dlp 2026.06.09, whose default YouTube player-client selection for
      # some videos is `android_vr`; YouTube's CDN answers those signed
      # stream URLs with HTTP 403, so playback fails with `Raw(-16)` /
      # "Failed to open" even though extraction succeeded. nixos-unstable
      # ships 2026.08.19, which selects the working `visionos` client for the
      # same video. This is a yt-dlp-extractor/client-selection issue, not a
      # castoff or mpv one -- see AGENTS.md. Everything else stays on
      # nixos-25.11, including mpv/libmpv and the Rust toolchain.
      castoffDaemonFor = system:
        let
          pkgs = import nixpkgs { inherit system; };
          yt-dlp = (import nixpkgs-unstable { inherit system; }).yt-dlp;
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "castoff-daemon";
          version = "0.1.0";
          src = ./daemon;
          cargoLock.lockFile = ./daemon/Cargo.lock;

          # libmpv2-sys links against libmpv via a bare `-lmpv` (it ships
          # pregenerated bindings, so no bindgen/libclang needed); nixpkgs'
          # cc-wrapper picks up mpv-unwrapped's lib dir automatically from
          # buildInputs.
          buildInputs = [ pkgs.mpv-unwrapped ];

          # `cargo test` (in the default checkPhase) has no network access in
          # the Nix build sandbox, which is fine: the one test that needs the
          # network (real YouTube playback, daemon/src/player.rs) is `#[ignore]`d
          # and run manually instead -- see README.
          #
          # cage/chromium/grim are test-only: they let the checkPhase run the
          # ignored end-to-end webpage-display test (see `postCheck` below),
          # which starts a real headless Cage session with the real Chromium
          # engine and asserts on compositor output captured via
          # `wlr-screencopy`.
          nativeBuildInputs = [ pkgs.makeWrapper pkgs.cage pkgs.chromium pkgs.grim ];

          # yt-dlp and Chromium are *runtime* dependencies, not build inputs:
          # (a) mpv's built-in ytdl_hook Lua script shells out to whatever
          #     `yt-dlp` it finds on `PATH` to resolve YouTube (and other
          #     non-direct-media) URLs -- see README's "How YouTube playback
          #     works";
          # (b) the daemon spawns Chromium as a second fullscreen client of the
          #     same Cage session to render web pages -- see README's "How
          #     webpage (dashboard) display works".
          # Wrap the binary so this is true regardless of the caller's
          # environment (e.g. the `cage` session that execs this binary
          # directly, no shell, on the real appliance).
          postFixup = ''
            wrapProgram $out/bin/castoff-daemon \
              --prefix PATH : ${pkgs.lib.makeBinPath [ yt-dlp pkgs.chromium ]}
          '';

          # The end-to-end test needs a compositor and a browser, which few
          # environments have, so it is `#[ignore]`d and thus skipped by the
          # `cargo test` the checkPhase runs by default; run it explicitly here,
          # where nativeBuildInputs provides both. `CASTOFF_E2E_SKIP_MPV_PIXELS`
          # tells the test what this sandbox cannot do: with no `/dev/dri`, Cage
          # falls back to wlroots' pixman renderer and mpv has no buffer-sharing
          # path to present frames through, so only mpv's own pixel assertions
          # are skipped (see `mpv_can_present` in
          # daemon/tests/webpage_display.rs). The engine, the compositor and the
          # page pixels asserted on are real regardless; `--nocapture` keeps all
          # of it in the build log if it ever fails.
          postCheck = ''
            CASTOFF_E2E_SKIP_MPV_PIXELS=1 \
              CASTOFF_E2E_BROWSER_FLAGS="--no-sandbox --disable-gpu" \
              cargo test --test webpage_display -- --ignored --nocapture
          '';

          # aarch64-linux is built under QEMU user-mode emulation on the common
          # x86_64-linux-with-binfmt path (there's no cross-compiled Rust
          # toolchain wired up here), and emulating the test binary -- above
          # all the headless Cage+Chromium e2e test in postCheck -- is
          # unreliable for verification: it can segfault the emulator itself
          # on some tests, unrelated to actual code correctness. Verification
          # for the Raspberry Pi 4 target happens on real Pi 4 hardware
          # instead of under emulation; x86_64-linux keeps its full checkPhase
          # unchanged. See AGENTS.md.
          doCheck = system != "aarch64-linux";

          meta = {
            description = "castoff TV-box daemon: drives mpv and exposes an FCast-based local control API";
            license = pkgs.lib.licenses.mit;
            mainProgram = "castoff-daemon";
          };
        };
      castoffDaemon = forEachSystem castoffDaemonFor;

      # The x86_64-linux appliance configuration. `nixos-rebuild build-vm
      # --flake .#tv-box` and `nix build .#tv-box-vm` both build *this*
      # configuration's `system.build.vm`, i.e. `virtualisation.vmVariant`
      # (see nix/tv-box.nix) extended with nixpkgs' qemu-vm module.
      # `tv-box-vm` used to be a second nixosConfiguration with qemu-vm.nix
      # wired in by hand, which meant the VM-only settings a `build-vm` run
      # needs could not live in the shared module and the two documented
      # commands were not actually equivalent.
      tv-box-system = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        specialArgs = { castoffDaemon = castoffDaemon.x86_64-linux; };
        modules = [ ./nix/tv-box.nix ./nix/tv-box-x86_64.nix ];
      };

      # The Raspberry Pi 4 appliance configuration: nix/tv-box.nix's
      # target-independent kiosk config layered onto nixos-raspberrypi's Pi 4
      # hardware modules via nix/tv-box-rpi4.nix. `nixosSystemFull` (rather
      # than the plainer `nixosSystem`) is the same "full" RPi-optimised
      # package set nixos-raspberrypi's own `nixosInstaller` uses -- this
      # flake used `nixosInstaller` originally, but that also pulls in
      # nixos-raspberrypi's `raspberrypi-installer.nix`, which imports
      # nixpkgs' `profiles/installation-device.nix`: the profile for
      # installation *media*, not a deployed appliance. Confirmed by reading
      # it directly, that profile force-enables `documentation.*` (overriding
      # `nix/tv-box.nix`'s deliberate closure-trimming), creates a
      # passwordless "nixos" user *and* a passwordless root account, sets
      # `services.getty.autologinUser = "nixos"`, and sets
      # `services.openssh.settings.PermitRootLogin = mkDefault "yes"` -- none
      # of that belongs on a deployed, SSH-reachable box. `nix/tv-box-rpi4.nix`
      # instead imports nixos-raspberrypi's `sd-image` module directly (the
      # actual flashability/auto-expanding-partition-table piece) without
      # `raspberrypi-installer.nix`, so the appliance gets a flashable image
      # with none of the installer-media side effects. See nix/tv-box-rpi4.nix
      # and README's Raspberry Pi 4 section.
      tv-box-rpi4-system = nixos-raspberrypi.lib.nixosSystemFull {
        specialArgs = { castoffDaemon = castoffDaemon.aarch64-linux; };
        modules = [ ./nix/tv-box.nix ./nix/tv-box-rpi4.nix ];
      };
    in
    {
      packages = forEachSystem (system:
        { castoff-daemon = castoffDaemon.${system}; default = castoffDaemon.${system}; }
        // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
          tv-box-vm = tv-box-system.config.system.build.vm;
        }
        // nixpkgs.lib.optionalAttrs (system == "aarch64-linux") {
          tv-box-rpi4-image = tv-box-rpi4-system.config.system.build.sdImage;
        });

      checks = forEachSystem (system: {
        castoff-daemon = castoffDaemon.${system};
      });

      apps = forEachSystem (system: {
        castoff-daemon = {
          type = "app";
          program = "${castoffDaemon.${system}}/bin/castoff-daemon";
        };
      });

      nixosConfigurations = {
        tv-box = tv-box-system;
        tv-box-rpi4 = tv-box-rpi4-system;
      };

      devShells.x86_64-linux.default =
        let
          pkgs = import nixpkgs { system = "x86_64-linux"; };
          yt-dlp = (import nixpkgs-unstable { system = "x86_64-linux"; }).yt-dlp;
        in
        pkgs.mkShell {
          inputsFrom = [ castoffDaemon.x86_64-linux ];
          # `inputsFrom` only pulls in `castoff-daemon`'s buildInputs/nativeBuildInputs
          # (mpv-unwrapped, makeWrapper); it does NOT carry over that package's
          # `postFixup` PATH wrapping. Without `yt-dlp` (and `chromium`) listed
          # here too, `cargo build`/`cargo run` inside this dev shell produces an
          # unwrapped binary with neither on `PATH`, so YouTube playback and web
          # page display would fail (mpv's ytdl_hook has nothing to shell out
          # to; the daemon has no browser engine to spawn) even though
          # `nix build` (which does apply the wrapper) works fine. Uses the
          # same nixos-unstable `yt-dlp` as the package wrapper (see above), so
          # tests run against the version the appliance actually ships.
          #
          # `cage` and `grim` are test-only: `daemon/tests/webpage_display.rs`
          # runs a real headless Cage session with the real Chromium engine and
          # asserts on compositor output captured over `wlr-screencopy`.
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.clippy
            yt-dlp
            pkgs.chromium
            pkgs.cage
            pkgs.grim
          ];
        };
    };
}
