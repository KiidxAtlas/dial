class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.3.15"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "03ba12af8a523368f6d85d8defb873e712bd2b5933000168163a3b57b197f0b0"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "4102ffc35dfe24b122c3d96af29818b3e918257d1ca6a6edf0cfabb965628cca"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "118ef64778e76e0955270472e7db0a928a0615fe49dbf62ff09e7c3a41de934a"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "1f217d8e2ccbac49af5dd58f2d5852b299e51e14fc3243ffff8da9a978362f4e"
    end
  end

  def install
    bin.install "dial"
    share.install "LICENSE", "README.md", "THIRD_PARTY_NOTICES", "Silver.LICENSE", "Unscii.LICENSE"
    # Older release archives do not carry both bundled interpreters.
    share.install Dir["LICENSE.node", "LICENSE.python"]
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/dial --version")
  end
end
