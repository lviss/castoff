{
  description = "castoff: an open-source, appliance-like Chromecast/Roku alternative";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    # Second nixpkgs, used for exactly one package: `yt-dlp`. See `yt-dlp`
    # below for why the stable pin isn't good enough. Nothing else here is
    # imported from this input.
    nixpkgs-unstable.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, nixpkgs-unstable }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };

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
      yt-dlp = (import nixpkgs-unstable { inherit system; }).yt-dlp;

      castoff-daemon = pkgs.rustPlatform.buildRustPackage {
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
        nativeBuildInputs = [ pkgs.makeWrapper ];

        # yt-dlp is a *runtime* dependency, not a build input: mpv's built-in
        # ytdl_hook Lua script shells out to whatever `yt-dlp` it finds on
        # `PATH` to resolve YouTube (and other non-direct-media) URLs -- see
        # README's "How YouTube playback works". Wrap the binary so this is
        # true regardless of the caller's environment (e.g. the `cage`
        # session that execs this binary directly, no shell, on the real
        # appliance).
        postFixup = ''
          wrapProgram $out/bin/castoff-daemon \
            --prefix PATH : ${pkgs.lib.makeBinPath [ yt-dlp ]}
        '';

        meta = {
          description = "castoff TV-box daemon: drives mpv and exposes an FCast-based local control API";
          license = pkgs.lib.licenses.mit;
          mainProgram = "castoff-daemon";
        };
      };

      # Separate nixosConfiguration, identical to `tv-box` but with the
      # qemu-vm module pulled in, so it can be built directly with
      # `nix build .#packages.x86_64-linux.tv-box-vm` without going through
      # `nixos-rebuild` -- useful in sandboxes/CI with no access to
      # `nixos-rebuild build-vm`. See README for both workflows.
      tv-box-vm-system = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { castoffDaemon = castoff-daemon; };
        modules = [
          ./nix/tv-box.nix
          "${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix"
          {
            virtualisation.graphics = true;
            virtualisation.memorySize = 2048;
          }
        ];
      };
    in
    {
      packages.${system} = {
        castoff-daemon = castoff-daemon;
        default = castoff-daemon;
        tv-box-vm = tv-box-vm-system.config.system.build.vm;
      };

      checks.${system} = {
        castoff-daemon = castoff-daemon;
      };

      apps.${system}.castoff-daemon = {
        type = "app";
        program = "${castoff-daemon}/bin/castoff-daemon";
      };

      nixosConfigurations.tv-box = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { castoffDaemon = castoff-daemon; };
        modules = [ ./nix/tv-box.nix ];
      };

      devShells.${system}.default = pkgs.mkShell {
        inputsFrom = [ castoff-daemon ];
        # `inputsFrom` only pulls in `castoff-daemon`'s buildInputs/nativeBuildInputs
        # (mpv-unwrapped, makeWrapper); it does NOT carry over that package's
        # `postFixup` PATH wrapping. Without `yt-dlp` listed here too,
        # `cargo build`/`cargo run` inside this dev shell produces an
        # unwrapped binary with no `yt-dlp` on `PATH`, so YouTube playback
        # silently fails (mpv's ytdl_hook has nothing to shell out to) even
        # though `nix build` (which does apply the wrapper) works fine. Uses
        # the same nixos-unstable `yt-dlp` as the package wrapper (see above),
        # so tests run against the version the appliance actually ships.
        packages = [ pkgs.cargo pkgs.rustc pkgs.rust-analyzer pkgs.clippy yt-dlp ];
      };
    };
}
