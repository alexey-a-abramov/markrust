# Homebrew formula template for MarkRust.
# Place this file in the alexey-a-abramov/homebrew-markrust tap repository.
# Update sha256 values after the first GitHub Release is published.

class Markrust < Formula
  desc "Fast native Markdown workspace for developers"
  homepage "https://github.com/alexey-a-abramov/markrust"
  version "0.1.0"
  license "MPL-2.0"

  on_macos do
    on_arm do
      url "https://github.com/alexey-a-abramov/markrust/releases/download/v0.1.0/markrust-macos-aarch64.tar.gz"
      sha256 "REPLACE_WITH_SHA256_AFTER_RELEASE"
    end
    on_intel do
      url "https://github.com/alexey-a-abramov/markrust/releases/download/v0.1.0/markrust-macos-x86_64.tar.gz"
      sha256 "REPLACE_WITH_SHA256_AFTER_RELEASE"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/alexey-a-abramov/markrust/releases/download/v0.1.0/markrust-linux-x86_64.tar.gz"
      sha256 "REPLACE_WITH_SHA256_AFTER_RELEASE"
    end
  end

  def install
    bin.install "markrust"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/markrust --version")
  end
end
