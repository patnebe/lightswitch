{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        flake-utils.follows = "flake-utils";
      };
    };
  };
  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ]
      (system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs {
            inherit system overlays;
          };
          elfutils-without-zstd = pkgs.elfutils.overrideAttrs (attrs: {
            configureFlags = attrs.configureFlags ++ [ "--without-zstd" ];
          });
          # Separate in other nix files.
          kernel_5_15 = pkgs.stdenv.mkDerivation {
            name = "download-kernel-5.15";
            src = pkgs.fetchurl {
              url = "https://github.com/danobi/vmtest/releases/download/test_assets/bzImage-v5.15-fedora38";
              hash = "sha256-nq8W72vuNKCgO1OS6aJtAfg7AjHavRZ7WAkP7X6V610=";
            };
            dontUnpack = true;
            installPhase = ''
              mkdir -p $out
              cp -r $src $out/bzImage
            '';
          };

          kernel_6_6 = pkgs.stdenv.mkDerivation {
            name = "download-kernel-6.6";
            src = pkgs.fetchurl {
              url = "https://github.com/danobi/vmtest/releases/download/test_assets/bzImage-v6.6-fedora38";
              hash = "sha256-6Fu16SPBITP0sI3lapkckZna6GKBn2hID038itt82jA=";
            };
            dontUnpack = true;
            installPhase = ''
              mkdir -p $out
              cp -r $src $out/bzImage
            '';
          };

          # Using the rust derivation to make nix aware of this package and use the cache.
          vmtest = pkgs.rustPlatform.buildRustPackage {
            name = "vmtest";
            src = pkgs.fetchFromGitHub {
              owner = "danobi";
              repo = "vmtest";
              rev = "51f11bf301fea054342996802a16ed21fb5054f4";
              sha256 = "sha256-qtTq0dnDHi1ITfQzKrXz+1dRMymAFBivWpjXntD09+A=";
            };
            cargoHash = "sha256-SHjjCWz4FVVk1cczkMltRVEB3GK8jz2tVABNSlSZiUc=";
            nativeCheckInputs = [ pkgs.qemu ];

            # There are various errors trying to access `/build/source/tests/*`.
            doCheck = false;

            meta = with pkgs.lib; {
              description = "Helps run tests in virtual machines";
              homepage = "https://github.com/danobi/vmtest/";
              license = licenses.asl20;
              mainProgram = "";
              maintainers = with maintainers; [ ];
              platforms = platforms.linux;
            };
          };


          vmtest-create-config = pkgs.stdenv.mkDerivation {
            name = "vmtest-dump-config";
            dontUnpack = true;

            # The flamegraph is written to the current directory, run lightswitch from /tmp to write it there.
            src = pkgs.writeText "vmtest.toml" ''
              [[target]]
              name = "Fedora 5.15"
              kernel = "${kernel_5_15.out}/bzImage"
              command = "target/x86_64-unknown-linux-gnu/debug/lightswitch --duration 2"

              [[target]]
              name = "Fedora 6.6"
              kernel = "${kernel_6_6.out}/bzImage"
              command = "target/x86_64-unknown-linux-gnu/debug/lightswitch --duration 2"
            '';
            nativeBuildInputs = [ vmtest kernel_5_15 kernel_6_6 ];
            installPhase = ''
              mkdir -p $out
              cp -r $src $out/vmtest.toml
            '';
          };

          # Requires lightswitch to be statically built using the x86_64-unknown-linux-gnu target. Definitely
          # should be done automatically.
          runvmtests = pkgs.stdenv.mkDerivation {
            name = "run-vmtests";
            dontUnpack = true;

            src = pkgs.writeText "run-vmtests" ''
              cargo build
              ${vmtest}/bin/vmtest --config ${vmtest-create-config}/vmtest.toml
            '';
            nativeBuildInputs = [ vmtest-create-config ];
            installPhase = ''
              mkdir -p $out/bin
              cp -r $src $out/bin/run-vmtests
              chmod +x $out/bin/run-vmtests
            '';
          };

        in
        with pkgs;
        {
          formatter = pkgs.nixpkgs-fmt;
          packages = rec {
            run-vm-tests = runvmtests;
          };


          devShells.default = mkShell rec {
            # https://discourse.nixos.org/t/how-to-add-pkg-config-file-to-a-nix-package/8264/4
            nativeBuildInputs = with pkgs; [
              pkg-config
            ];
            buildInputs = [
              rust-bin.stable.latest.default
              llvmPackages_16.clang
              # llvmPackages_16.clang-unwrapped https://github.com/NixOS/nixpkgs/issues/30670
              llvmPackages_16.libcxx
              llvmPackages_16.libclang
              llvmPackages_16.lld
              # Debugging
              strace
              gdb
              # Native deps
              glibc
              glibc.static
              elfutils-without-zstd
              zlib.static
              zlib.dev
              openssl
              # Other tools
              cargo-edit
              # ocamlPackages.magic-trace
            ];

            LIBCLANG_PATH = lib.makeLibraryPath [ llvmPackages_16.libclang ];
            LD_LIBRARY_PATH = lib.makeLibraryPath [ zlib.static elfutils-without-zstd ];
          };
        }
      );
}
