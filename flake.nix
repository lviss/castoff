{
  description = "castoff: an open-source, appliance-like Chromecast/Roku alternative";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };

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
        packages = [ pkgs.cargo pkgs.rustc pkgs.rust-analyzer pkgs.clippy ];
      };
    };
}
