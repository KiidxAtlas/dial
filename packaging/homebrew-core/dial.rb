class Dial < Formula
  desc "Coding agent with undo, test evidence and an optional sandbox"
  homepage "https://dial.rootatlas.dev/"
  url "https://github.com/KiidxAtlas/dial/archive/refs/tags/v0.3.1.tar.gz"
  sha256 "e9eeac0d7e8535270ee018d7ad1aae1f01537cb043642a334d98b33ddd0f8b73"
  license "MIT"
  head "https://github.com/KiidxAtlas/dial.git", branch: "main"

  depends_on "llvm" => :build
  depends_on "pkgconf" => :build
  depends_on "rust" => :build

  on_linux do
    depends_on "bubblewrap"
  end

  def install
    ENV["LIBCLANG_PATH"] = formula_opt_lib("llvm").to_s
    ENV["DIAL_BUILD_REVISION"] = "v#{version}"
    system "cargo", "install", *std_cargo_args(path: ".")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/dial --version")
  end
end
