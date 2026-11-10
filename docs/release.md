# Releasing murmur

murmur v0.1 is distributed from its own signed Flatpak repo on GitHub Pages, plus a GitHub
Release ([D-015](decisions.md)). `.github/workflows/release.yml` does the work on every `v*`
tag. These steps are for the maintainer. They need your accounts and keys, so Claude doesn't
do them (D-004: Claude never pushes).

## One-time setup

1. **Push the repository** to `github.com/gabus/murmur` (public).
2. **Create a signing key** used only for the Flatpak repo, on your machine:
   ```sh
   gpg --quick-gen-key "murmur Flatpak repo <you@example.org>" rsa4096 sign never
   gpg --list-secret-keys --keyid-format long      # note the key id
   gpg --armor --export-secret-keys <KEYID> > murmur-repo-key.asc
   ```
   Keep `murmur-repo-key.asc` safe. Users' Flatpak installs trust this key's public half
   forever, so losing it means asking everyone to reinstall.
3. **Add it as a secret:** GitHub → Settings → Secrets and variables → Actions → New
   repository secret, named `FLATPAK_GPG_KEY`, with the contents of `murmur-repo-key.asc`.
   Then delete the file from disk, or store it offline.
4. **Enable Pages:** GitHub → Settings → Pages → Build and deployment → Source: **GitHub
   Actions**.

## Each release

1. Update the version in `Cargo.toml` and add a `<release>` entry in
   `data/io.github.gabus.Murmur.metainfo.xml`, then commit.
2. If `Cargo.lock` changed since the last release, run `flatpak/update-cargo-sources.sh`
   first (README → Build & run).
3. Tag and push: `git tag v0.1.0 && git push origin main v0.1.0`.
4. The **Release** workflow builds the Flatpak offline (about 10 min cold, faster with its
   cache), signs the repo, publishes `https://gabus.github.io/murmur/` (landing page,
   `murmur.flatpakref`, the repo) and creates the GitHub Release with `murmur.flatpak`,
   `murmur.flatpakref` and `murmur.flatpakrepo`.
5. Check it on a clean user: `flatpak install --user https://gabus.github.io/murmur/murmur.flatpakref`.

**Status:** the workflow has not run yet (it was written without GitHub access). Expect to fix
small things on the first tag.

## Flathub (later, optional)

Flathub's Generative AI policy forbids AI-generated or AI-assisted content in Flathub
manifests. It also requires disclosing AI-generated code, and the submission PR must be opened
by a person. To submit, write the Flathub manifest yourself (`io.github.gabus.Murmur.yml` and
`flatpak/` are a tested reference) and use a tagged git source instead of `type: dir`. Other
linter items are listed in [packaging.md](packaging.md).
