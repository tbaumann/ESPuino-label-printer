{
  lib,
  rustPlatform,
  fetchFromGitHub,
  installShellFiles,
}:
# Markie: the markdown backend a card's `markdown` back is rendered with. The
# server shells out to it (see the markie entry points in server/src/render.rs).
#
# Built from source rather than from upstream's release archives: the crate is
# pure Rust, so pinning its binaries buys nothing, and one code path then covers
# every system the flake declares instead of a per-platform asset table.
#
# Fonts are a RUNTIME concern here, not a build input: markie renders with the
# fonts it finds (system fonts via fontdb's fontconfig reading, plus a `fonts`
# directory in the working directory) and refuses to start when it finds none.
# Nothing below supplies them — a caller that renders text needs fonts in its
# own environment, which is why the NixOS tests that exercise a markdown back
# must carry a font package.
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "markie";
  version = "0.6.2";

  src = fetchFromGitHub {
    owner = "lsj5031";
    repo = "MarkieCli";
    tag = "v${finalAttrs.version}";
    hash = "sha256-TZpRFigtsOyUQL/F51fs3M7TFWMib9HpfGiNUYnYjLk=";
  };

  cargoHash = "sha256-kETepzkJzEuff+no/o/wGnP3cOUdtdW55b8YrSxoZKo=";

  nativeBuildInputs = [installShellFiles];

  # Both binaries are the upstream CLI: `markie` for a document,
  # `markie-mermaid` for a standalone .mmd.
  postInstall = ''
    installShellCompletion --cmd markie \
      --bash <($out/bin/markie --completions bash) \
      --fish <($out/bin/markie --completions fish) \
      --zsh <($out/bin/markie --completions zsh)
  '';

  # Upstream's suite includes visual fixtures compared against demo assets that
  # were rendered in a specific font environment, so it cannot hold in a build
  # sandbox (and the dev-dependencies would only be built to run it).
  doCheck = false;

  meta = {
    description = "Markdown to SVG/PNG/PDF renderer with native Mermaid diagrams and LaTeX math";
    homepage = "https://github.com/lsj5031/MarkieCli";
    license = lib.licenses.mit;
    mainProgram = "markie";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
})
