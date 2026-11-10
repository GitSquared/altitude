class Altitude < Formula
  desc "Terminal control plane for delegated coding agents"
  homepage "https://github.com/GitSquared/altitude"
  url "https://github.com/GitSquared/altitude/releases/download/v0.8.0/altitude-0.8.0-aarch64-apple-darwin.tar.gz"
  sha256 "63518fb11a54f52498ea5250e40fa687c530f1001bc7e9c0ae793dbc68b71f93"
  license "BUSL-1.1"
  version "0.8.0"

  depends_on :macos
  depends_on arch: :arm64

  def install
    # Homebrew stages a single-root archive inside that root, so the binary is either
    # here or one directory down; an archive that renames its root still installs.
    bin.install Dir["altitude", "*/altitude"].first
  end

  test do
    assert_match "altitude v#{version}", shell_output("#{bin}/altitude --version")
  end
end
