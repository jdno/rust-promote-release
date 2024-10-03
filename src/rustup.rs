use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Error};
use curl::easy::Easy;
use serde::Deserialize;

use crate::config::Channel;
use crate::curl_helper::BodyExt;
use crate::{run, Context};

impl Context {
    /// Promote a `rustup` release
    ///
    /// The [release process] for `rustup` involves copying existing artifacts from one S3 bucket to
    /// another, updating the manifest, and archiving the artifacts for long-term storage.
    ///
    /// `rustup` uses different branches to manage releases. Whenever a commit is pushed to the
    /// `stable` branch in [rust-lang/rustup], GitHub Actions workflows build release artifacts and
    /// copy them into `s3://rustup-builds/builds/${commit-sha}/`.
    ///
    /// When a new release is cut and this method is invoked, it downloads the artifacts from that
    /// bucket (which must always be set as the `DOWNLOAD_BUCKET` variable). A copy of the artifacts
    /// is archived in `s3://${UPLOAD_BUCKET}/rustup/archive/${version}/`, where `version` is
    /// derived from the Cargo.toml file in the `stable` branch. `UPLOAD_BUCKET` can either be the
    /// `dev-static` or the `static` bucket.
    ///
    /// If the release is for the `stable` channel, the artifacts are also copied to the `dist/`
    /// path in the `UPLOAD_BUCKET` bucket. The `dist/` path is used by the `rustup` installer to
    /// download the latest release.
    ///
    /// Then, the `release-stable.toml` manifest is updated with the new version and copied to
    /// `s3://${UPLOAD_BUCKET}/rustup/release-stable.toml`.
    ///
    /// [release process]: https://rust-lang.github.io/rustup/dev-guide/release-process.html
    /// [rust-lang/rustup]: https://github.com/rust-lang/rustup
    pub fn promote_rustup(&mut self) -> anyhow::Result<()> {
        // Rustup only has beta and stable releases, so we fail fast when trying to promote nightly
        self.enforce_rustup_channel()?;

        // Get the latest commit from the `stable` branch or use the user-provided override
        let head_sha = self.get_commit_sha_for_rustup_release()?;

        // The commit on the `stable` branch is used to determine the version number
        let version = self.get_next_rustup_version(&head_sha)?;

        // Download the Rustup artifacts from S3
        let dist_dir = self.download_rustup_artifacts(&head_sha)?;

        // Archive the artifacts
        self.archive_rustup_artifacts(&dist_dir, &version)?;

        if self.config.channel == Channel::Stable {
            // Promote the artifacts to the release bucket
            self.promote_rustup_artifacts(&dist_dir)?;
        }

        // Update the release number
        self.update_rustup_release(&version)?;

        Ok(())
    }

    fn enforce_rustup_channel(&self) -> anyhow::Result<()> {
        println!("Checking channel...");

        if self.config.channel != Channel::Stable && self.config.channel != Channel::Beta {
            return Err(anyhow!(
                "promoting rustup is only supported for the stable and beta channels"
            ));
        }

        Ok(())
    }

    fn get_commit_sha_for_rustup_release(&self) -> anyhow::Result<String> {
        match &self.config.override_commit {
            Some(sha) => Ok(sha.clone()),
            None => self.get_head_sha_for_rustup(),
        }
    }

    fn get_head_sha_for_rustup(&self) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Commit {
            sha: String,
        }

        let url = format!(
            "https://api.github.com/repos/rust-lang/rustup/commits/{}",
            self.config.channel
        );

        let mut client = Easy::new();
        client.url(&url)?;
        client.useragent("rust-lang/promote-release")?;

        let commit: Commit = client.without_body().send_with_response()?;

        Ok(commit.sha)
    }

    fn get_next_rustup_version(&self, sha: &str) -> anyhow::Result<String> {
        println!("Getting next Rustup version from Cargo.toml...");

        #[derive(Deserialize)]
        struct Content {
            content: String,
        }

        #[derive(Deserialize)]
        struct CargoToml {
            package: Package,
        }

        #[derive(Deserialize)]
        struct Package {
            version: String,
        }

        let url =
            format!("https://api.github.com/repos/rust-lang/rustup/contents/Cargo.toml?ref={sha}");

        let mut client = Easy::new();
        client.url(&url)?;
        client.useragent("rust-lang/promote-release")?;

        let content: Content = client.without_body().send_with_response()?;
        let decoded_content = base64::decode(content.content.replace('\n', ""))?;
        let cargo_toml = String::from_utf8(decoded_content)?;

        let toml: CargoToml = toml::from_str(&cargo_toml)?;

        Ok(toml.package.version)
    }

    fn download_rustup_artifacts(&mut self, sha: &str) -> Result<PathBuf, Error> {
        println!("Downloading artifacts from dev-static...");

        let dl = self.dl_dir().join("dist");
        // Remove the directory if it exists, otherwise just ignore.
        let _ = fs::remove_dir_all(&dl);
        fs::create_dir_all(&dl)?;

        let download_path = format!("{}/{}", self.config.download_dir, sha);

        run(self
            .aws_s3()
            .arg("cp")
            .arg("--recursive")
            .arg("--only-show-errors")
            .arg(self.s3_artifacts_url(&download_path))
            .arg(format!("{}/", dl.display())))?;

        Ok(dl)
    }

    fn archive_rustup_artifacts(&mut self, dist_dir: &Path, version: &str) -> Result<(), Error> {
        println!("Archiving artifacts for version {version}...");

        let path = format!("archive/{}/", version);

        self.upload_rustup_artifacts(dist_dir, &path)
    }

    fn promote_rustup_artifacts(&mut self, dist_dir: &Path) -> Result<(), Error> {
        println!("Promoting artifacts to dist/...");

        let release_bucket_url = format!(
            "s3://{}/{}/dist/",
            self.config.upload_bucket, self.config.upload_dir,
        );

        run(self
            .aws_s3()
            .arg("cp")
            .arg("--recursive")
            .arg("--only-show-errors")
            .arg(format!("{}/", dist_dir.display()))
            .arg(&release_bucket_url))
    }

    fn upload_rustup_artifacts(&mut self, dist_dir: &Path, target_path: &str) -> Result<(), Error> {
        run(self
            .aws_s3()
            .arg("cp")
            .arg("--recursive")
            .arg("--only-show-errors")
            .arg(format!("{}/", dist_dir.display()))
            .arg(format!(
                "s3://{}/{}/{}",
                self.config.upload_bucket, self.config.upload_dir, target_path
            )))
    }

    fn update_rustup_release(&mut self, version: &str) -> Result<(), Error> {
        println!("Updating version and manifest...");

        let manifest_path = self.dl_dir().join("release-stable.toml");
        let manifest = format!(
            r#"
schema-version = '1'
version = '{}'
            "#,
            version
        );

        fs::write(&manifest_path, manifest)?;

        run(self
            .aws_s3()
            .arg("cp")
            .arg("--only-show-errors")
            .arg(manifest_path)
            .arg(format!(
                "s3://{}/{}/release-stable.toml",
                self.config.upload_bucket, self.config.upload_dir
            )))
    }
}
