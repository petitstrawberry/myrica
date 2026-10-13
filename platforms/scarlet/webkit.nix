{ pkgs, rustToolchain, runtime, scarletSource, webkitSource }:
let
  lib = pkgs.lib;
  dependencies = import (webkitSource + "/Source/WebCore/scarlet/Dependencies/default.nix") {
    inherit pkgs runtime;
  };
  brotli = (builtins.fetchTree {
    type = "github";
    owner = "google";
    repo = "brotli";
    rev = "ed738e842d2fbdf2d6459e39267a633c4a9b2f5d";
  }).outPath;
  woff2 = (builtins.fetchTree {
    type = "github";
    owner = "google";
    repo = "woff2";
    rev = "1bccf208bca986e53a647dfe4811322adb06ecf8";
  }).outPath;
  fontSources = pkgs.linkFarm "scarlet-webkit-font-sources" [
    { name = "brotli"; path = brotli; }
    { name = "woff2"; path = woff2; }
  ];
  clang = pkgs.llvmPackages_21.clang-unwrapped;
  llvm = pkgs.llvmPackages_21.llvm;
in pkgs.stdenv.mkDerivation {
  pname = "scarlet-webkit";
  version = "1";
  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./build-webkit.py ./font_dependencies.py ];
  };
  nativeBuildInputs = [
    rustToolchain clang llvm pkgs.llvmPackages_21.lld
    pkgs.python3 pkgs.cmake pkgs.ninja pkgs.meson pkgs.pkg-config
    pkgs.perl pkgs.ruby pkgs.gperf pkgs.bison pkgs.flex pkgs.tcl
  ];
  dontConfigure = true;
  dontFixup = true;
  buildPhase = ''
    runHook preBuild
    python3 build-webkit.py --scarlet-source ${scarletSource} \
      --webkit-source ${webkitSource} --runtime ${runtime} --output "$PWD/build" \
      --dependencies ${dependencies} --font-sources ${fontSources} \
      --cc ${clang}/bin/clang --cxx ${clang}/bin/clang++ \
      --ar ${llvm}/bin/llvm-ar --ranlib ${llvm}/bin/llvm-ranlib \
      --jobs "$NIX_BUILD_CORES"
    runHook postBuild
  '';
  installPhase = ''
    mkdir -p "$out"
    cp -R build/prefix/. "$out/"
    cp -R build/fonts-prefix/. "$out/"
    cp -R build/memory-prefix/. "$out/"
    substituteInPlace "$out/share/scarlet/ScarletWebView.link" \
      --replace-fail "$PWD/build/prefix" "$out" \
      --replace-fail "$PWD/build/fonts-prefix" "$out" \
      --replace-fail "$PWD/build/memory-prefix" "$out"
  '';
  passthru = { inherit dependencies; };
}
