# Website deployment and custom domain

This document covers the static product site in `website/`. Binary release
archives are handled separately by [the release workflow](../.github/workflows/release.yml).

## Current state

As checked on 2026-09-14, `markrust.org` did not resolve to an A, AAAA, or
CNAME record from the release workstation. The repository now includes a
GitHub Pages workflow at
[`website-pages.yml`](../.github/workflows/website-pages.yml), which builds
the Astro site from `main` and deploys its `website/dist` artifact.

The workflow alone cannot activate GitHub Pages or change DNS. Those actions
require repository-admin access and control of the `markrust.org` DNS zone.
Confirm or establish domain registration with a registrar first; a DNS lookup
alone cannot prove who controls the domain.

## One-time GitHub Pages setup

1. Confirm that the registrar account controls `markrust.org` and its DNS
   zone. If it is not registered, register it before continuing.
2. [Verify the custom domain in GitHub Pages](https://docs.github.com/en/pages/configuring-a-custom-domain-for-your-github-pages-site/verifying-your-custom-domain-for-github-pages)
   from the account's **Settings → Pages** page (not repository settings).
   Add the exact `_github-pages-challenge-…` TXT record GitHub generates to
   the DNS zone, and leave it in place.
3. In the repository, open **Settings → Pages**, select **GitHub Actions** as
   the publishing source, then set the custom domain to `markrust.org`.
4. In **Settings → Secrets and variables → Actions → Variables**, set
   `PAGES_CUSTOM_DOMAIN_READY` to `true`. This is a deliberate deployment
   interlock: it keeps the workflow from publishing root-absolute links to the
   default project URL before the custom domain is configured.
5. Only after steps 1–4, push to `main` or use **Run workflow** from `main`.
   Then add the DNS records below.
6. Enable HTTPS enforcement after GitHub has provisioned the certificate.

## DNS records

At the DNS provider, add all four IPv4 records for the apex domain. Add the
IPv6 records if the provider supports them, and point `www` at the account's
GitHub Pages hostname so GitHub can redirect it to the canonical apex domain.

| Type | Host | Value |
| --- | --- | --- |
| A | `@` | `185.199.108.153` |
| A | `@` | `185.199.109.153` |
| A | `@` | `185.199.110.153` |
| A | `@` | `185.199.111.153` |
| AAAA | `@` | `2606:50c0:8000::153` |
| AAAA | `@` | `2606:50c0:8001::153` |
| AAAA | `@` | `2606:50c0:8002::153` |
| AAAA | `@` | `2606:50c0:8003::153` |
| CNAME | `www` | `alexey-a-abramov.github.io` |

Review existing records before changing them. Replace only web-hosting records
that conflict with the Pages apex records; do not delete MX, TXT, mail, or
other unrelated service records. Do not use wildcard DNS records with GitHub
Pages. DNS propagation and certificate provisioning can take up to 24 hours.

## Verification

```bash
dig markrust.org A +noall +answer
dig markrust.org AAAA +noall +answer
dig www.markrust.org CNAME +noall +answer
curl -I https://markrust.org
```

The `A` and `AAAA` results should match the table, `www` should point to the
GitHub Pages hostname, and HTTPS should return a successful response without a
certificate warning.

## Related

- [Engineering documentation index](README.md) — documentation hub
- [Project roadmap](../ROADMAP.md) — release gates and priorities
- [GitHub Pages custom domains](https://docs.github.com/en/pages/configuring-a-custom-domain-for-your-github-pages-site/managing-a-custom-domain-for-your-github-pages-site) — current provider guidance
