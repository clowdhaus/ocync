---
title: Security design
description: How ocync handles cryptography, cloud authentication and supply chain, measured against a FedRAMP High bar
order: 5
---

## The bar

`ocync` targets regulated environments: FIPS, GovCloud and DoD. FedRAMP High is the bar each decision is measured against. Three principles follow from it:

- **Every cryptographic operation runs inside the FIPS module.** TLS, request signing, token signing and digests all go through `aws-lc-rs` built against the FIPS module. No pure-Rust cryptography in a FIPS build.
- **Own the code on the security path.** Cloud authentication is a small surface: a few API calls and a credential chain. Owning it keeps the dependency tree small and every cryptographic call on the FIPS module.
- **Every release carries evidence.** Signatures, provenance and a bill of materials, so an assessor can check what shipped.

## Current state

- `aws-lc-rs` is the only TLS provider. `ring`, `openssl` and `native-tls` are banned in `deny.toml`.
- Linux release binaries are built with the `fips` feature, statically linked against glibc. See [FIPS 140-3](/fips).
- Content digests use `aws-lc-rs` (`crates/ocync-distribution/src/sha256.rs`).
- Release binaries and images carry build provenance and a bill of materials attestation. Images are also signed with cosign. GitHub Actions are pinned to commit SHAs.
- The Docker base images (`cgr.dev/chainguard/rust:latest-dev`, `cgr.dev/chainguard/static:latest`) float by design so every build picks up Chainguard's patches. They are not pinned by digest.

## Dependency weight

The FIPS build of the `ocync` binary resolves 161 crates (normal and build dependencies), down from 201 with `google-cloud-auth`. No cloud SDK crate is in the binary, and neither are the pure-Rust `sha2` and `hmac` crates or a pure-Rust ChaCha20 generator. `xtask`'s benchmark tooling still uses `aws-config` and `aws-sdk-ecr`, and is not shipped.

## TLS uses only FIPS-approved algorithms

The `fips` feature enables `rustls/fips`. That feature applies to every `rustls` user in the build, so `aws_lc_rs::default_provider()` drops ChaCha20-Poly1305 and every key exchange group the module does not approve, for `reqwest`, which the AWS and Google authentication also use. TLS 1.2 also requires extended master secret.

At startup a FIPS build checks `tls_fips()`, which asks `rustls` whether the installed provider is FIPS-approved, including whether the module is in approved mode. Every command except `version` refuses to start if it is not, so `version` can still report the failing state. `ocync version` prints the AWS-LC-FIPS module number a certificate names (`module: AWS-LC-FIPS 4`), the AWS-LC release (`release: AWS-LC 4.2.0`) and the result (`tls: approved`), in the shape `openssl list -providers` uses. A FIPS build decides all of this from one constant, `CRYPTO_BUILD` in `ocync-distribution`, so the CLI cannot disagree with the linked module.

`tests/fips_tls.rs` in `ocync-distribution` pins the behavior: the registry client fails the handshake against servers offering only TLS 1.3 ChaCha20-Poly1305, only TLS 1.2 ChaCha20-Poly1305, or only X25519, and succeeds against one offering AES-128-GCM.

## The shipped module's status is stated correctly

`ocync` links AWS-LC FIPS module 4, which AWS has submitted to NIST and which awaits certification. The docs name module 4 and its status, not a certificate it does not hold yet.

## Releases carry a bill of materials and provenance

Release binaries and the image's binary are built with `cargo auditable`, which embeds the dependency list in the binary, so scanners such as grype and trivy read it from the shipped artifact.

That embedded list comes from `cargo metadata`, which over-reports in two ways. It lists an optional dependency whenever a weak feature (`dep?/feature`, such as `quinn?/rustls-aws-lc-rs` in `reqwest`) names it, even when nothing enables it, and it resolves features across the whole workspace ([rust-lang/cargo#7754](https://github.com/rust-lang/cargo/issues/7754)), so it lists crates only other members compile. In the 0.8.2 FIPS build that adds 24 crates `ocync` does not compile: 22 that nothing in the workspace compiles, including `quinn`, `chrono` and `ring`, and 2 that `bench-proxy` and `xtask` bring in, `untrusted` 0.7 and `httpdate`. None of them leaves a source path in the binary, while compiled crates such as `rustls` and `tokio` do. A FIPS build also lists `aws-lc-sys`, which `rustls` resolves but the binary does not link: its only AWS-LC release string is the FIPS one.

`tools/sbom` turns syft's CycloneDX output into the release bill of materials:

- It keeps only the crates `cargo tree` compiles into `ocync` for that build, the same cross-check [cargo-auditable#210](https://github.com/rust-secure-code/cargo-auditable/pull/210) proposes upstream, and drops `aws-lc-sys` from a FIPS build.
- It names the linked C library, read from the binary: `AWS-LC FIPS 4.2.0`, marked as FIPS 140-3 module 4, or `AWS-LC 5.11.0` (`pkg:github/aws/aws-lc@v5.11.0`).
- It fails CI and the release when the binary links the wrong AWS-LC build, or when the bill of materials lacks `ocync` at the source tree's version, `aws-lc-rs` or the build's AWS-LC crate. That catches a binary built without its dependency list, a stale binary, and a FIPS binary built without the FIPS module.

What ships:

- Each binary has `<binary>.cdx.json`, attested on GitHub by the job that built it. `sha256sums.txt` and the build provenance cover the binaries and these documents.
- Each platform image carries a cosign attestation of its bill of materials, which also lists the base image's packages. The multi-arch index carries none, since one document cannot describe two platforms.
- Build provenance covers each platform image and the index, stored on GitHub and keyed by digest.
- Nothing is pushed to the registry until every binary and image passes its check.

Scanners that read the embedded list directly still see the dropped crates until cargo-auditable#210 ships, at which point the pinned cargo-auditable version is bumped.

## Owned Google authentication

`ocync` resolves Google Application Default Credentials itself, in `crates/ocync-distribution/src/google/`, as `google-cloud-auth` 1.18.0 does, which hashed and signed with the pure-Rust `sha2` and `hmac` crates and drew randomness from a pure-Rust ChaCha20 generator. Every hash, signature and cryptographic random number now comes from `aws-lc-rs`. The sources, in upstream's order:

- The file `GOOGLE_APPLICATION_CREDENTIALS` names, which must exist, then gcloud's well-known file (`$HOME/.config/gcloud/application_default_credentials.json`, or `%APPDATA%\gcloud\application_default_credentials.json` on Windows), then the GCE and GKE metadata server, at `GCE_METADATA_HOST` when it is set. When none of these exists and the machine is not on Google Cloud, there is no source at all (see the lifecycle table below).
- Service account keys: a self-signed RS256 JWT, signed with an `aws-lc-rs` RSA key and used directly as the access token, as upstream uses it. Only PKCS#8 keys are accepted, as upstream.
- Authorized users (`gcloud auth application-default login`): the refresh token exchanged at the file's `token_uri`.
- External accounts (Workload Identity Federation), with the subject token from a file (such as a projected Kubernetes service account token), a URL, or AWS, exchanged at Google's STS, then for a service account's token when `service_account_impersonation_url` is set.
- Impersonated service accounts (`impersonated_service_account`), with any of the above as the source and `delegates` passed on.

### Token lifecycle

The rules every change to `auth/gcp.rs` and `google/` keeps. Each rule names the cost it bounds, as requests to Google count against the run's rate limits.

| Event | What happens | Requests sent |
|---|---|---|
| First Artifact Registry or Container Registry host set up in a run | The credentials load once. Hosts set up at once wait on the same load. | None to Google. At most the AWS region lookup, to the instance metadata service |
| A later host in the run | Shares the load and the Google token | None |
| No source: no `GOOGLE_APPLICATION_CREDENTIALS`, no gcloud file, no `GCE_METADATA_HOST`, and a machine that is not on Google Cloud: its DMI product name (`/sys/class/dmi/id/product_name`) is not `Google` or `Google Compute Engine`, the test Google's Go `compute/metadata` library applies first, and one probe, `GET http://169.254.169.254/` with `Metadata-Flavor: Google` and a 2 second limit (Go's dial timeout), gets no answer carrying that header. The probe finds Cloud Run and Cloud Functions, which have no Google DMI, and says no on EC2, whose metadata service answers on the same IP without the header | Decided once at load and kept for the run. A registry chosen by its hostname is then read anonymously, with one INFO line, and one the user set with `auth_type: gar` or `gcr` asks the metadata server as before. This goes beyond upstream, which never falls back and fails here. A source that exists but fails to load or is refused, and a metadata server that fails on Google Cloud, are never read anonymously. Go races that probe against a DNS lookup of `metadata.google.internal`; `ocync` asks the IP alone, once, with no retry. Windows is taken to be on Google Cloud | One probe, only when no other source exists and the DMI name is not Google's. A machine off Google Cloud waits at most 2 seconds |
| The load fails | Kept for the run: every request of the load has already retried a transient failure | None more |
| A Google token is needed | The kept one, until 30 seconds before its `expires_in` or `expireTime`. One with no expiry is kept until an exchange refuses it, as upstream never refreshes one | One token chain per lifetime |
| A token fetch fails | Hosts that were waiting on it get its error. A host that asks after it tries again | One token chain per wave of waiting hosts |
| A request of the chain fails transiently (an HTTP 5xx, 408 or 429, a request that did not connect or complete, or an answer cut short) | That request alone is sent again on the crate's jittered schedule, up to 4 times, as the prepare phase reaches it before the engine's retry exists. A GCE or GKE metadata server that does not connect is retried, as GKE's takes a few seconds to start serving a new pod, so on a host with none each wave of waiting hosts fails about 4 seconds later. The EC2 instance metadata service a `region_url` names is up whenever its host is, so one that does not connect, or answer within 2 seconds, is final. A URL source on this host is retried, as a token sidecar may still be starting | At most 4 more of that request |
| A registry answers a request with 401 | The host's registry tokens that are the refused one are dropped, and its cached challenge with them when one matched. A token that has replaced the refused one is kept, and the Google token is never touched. An exchange already in flight for a scope whose token was dropped is detached: it serves the requests already waiting on it but stores nothing, and a later request starts its own, as the exchange may have begun from the cleared challenge. When no kept token was the refused one, nothing is detached and later requests join the exchange in flight. The next request exchanges again with the kept Google token | None |
| The token exchange answers 401 | The Google token itself was refused, as when it was revoked or Google's clock has passed it. Every request refused with it is dropped if still the one kept, and tried once more with the token kept now, a fresh one for the first and the replacement for the rest. If the host refuses the replacement too, the identity is refused, and the host's later 401s for that same token fetch no other. A token kept later, such as one refreshed at expiry, is tried the same way | One token chain per host per Google token |
| The token exchange answers 403 or 404 | A scope the identity cannot reach, or a missing repository, both measured on Artifact Registry. The Google token is kept | None |
| The next run, such as the next watch cycle | Everything is loaded afresh | One token chain |

A registry 401 judges the registry token only. Each cloud credential is judged where it is used, by the answer of the endpoint that takes it:

| Provider | Credential | Judged by | When refused |
|---|---|---|---|
| ECR | The `GetAuthorizationToken` token, which is also the registry credential | The registry's 401 for that token | Dropped if still the one kept, so the next request fetches a new one. Concurrent refusals of one stale token cost one `GetAuthorizationToken` |
| ECR Public | The `GetAuthorizationToken` credentials | The token endpoint's 401 on the exchange | Every request refused with them is sent once more with the ones kept now, after they are dropped if still the ones kept. If the replacement is refused too the request fails, and later requests fail on it without fetching more until a refresh yields different ones |
| Google (Artifact Registry, Container Registry) | The Google token | The token endpoint's 401 on the exchange | As the rows above |
| ACR | The Azure AD token, from the credential chain | `/oauth2/exchange` answering 401 | The credential chain is reset, unless it was reset since the token was asked for |
| ACR | The refresh token | `/oauth2/token` answering 401. A 403 or 404, as for one repository the identity cannot reach, judges nothing | Every request refused with it is sent once more with the one kept now. The first to be refused drops it if still the one kept and resets the credential chain. If the replacement is refused too the request fails, and later requests fail on it without fetching more until a refresh yields a different one |

A refresh begun before a credential was refused does not store that credential if it returns it, though the callers waiting on it still get it. A refresh begun after the refusal stores it: the source is issuing it again.

The upstream tests of the behavior `ocync` keeps are ported against the owned code, with an `aws-lc-rs` key made for the test where upstream makes one, and the AWS subject token's signature is checked against one computed independently. While both implementations existed, a differential test ran both against the same mock endpoints, comparing the requests sent (method, path, query, headers and body) and the tokens returned. All six cases matched: an authorized user, an external account with a JSON file source, client authentication and impersonation, an external account with a URL source and a workforce pool project, an impersonated account with delegates and an authorized user source, the metadata server, and a service account JWT's header and claims.

Deliberate differences from upstream:

- AWS subject token: signed with the owned AWS signer and credentials from `ocync`'s AWS chain for that region, so IRSA, EKS Pod Identity, profiles and every other AWS source work. Upstream reads only the `AWS_*` key variables and the instance metadata role URL the file names, which `ocync` does not read. The region comes from `AWS_REGION`, `AWS_DEFAULT_REGION`, the selected AWS profile's `region` (which upstream does not read), then the file's `region_url`, which is not asked when `AWS_EC2_METADATA_DISABLED` is set, as the AWS chain does not ask the instance metadata service then either. It is resolved when the registry is set up.
- The metadata server and the `region_url` lookup use no proxy, so a token never reaches a proxy host, and are bounded: 1 second to connect, 10 seconds per read. Upstream honors the system proxy and sets no timeout. A URL subject token source at a link-local or loopback address, the EC2 instance metadata service's IPv6 address, `localhost` or `metadata.google.internal`, such as Azure's instance metadata service or a sidecar, uses no proxy either. The other Google calls, including any other URL source, honor `HTTPS_PROXY` and `NO_PROXY`, as upstream, with 5 seconds to connect and 20 seconds per read.
- No call follows a redirect, so a body carrying a secret is never sent on to another host.
- `GCE_METADATA_HOST` must be a bare `host[:port]`. Upstream appends the token path to whatever it holds, so a path, query or user info in it sends the request somewhere else.
- Retries follow the [token lifecycle](#token-lifecycle). Upstream tries most calls once and retries only the IAM call.
- An impersonated token whose `expireTime` has already passed is an error. Upstream panics.
- A subject token format of type `json` with no `subject_token_field_name` is an error when the credentials load. Upstream reads an empty field name and fails at every exchange.
- STS client authentication sends `id:secret` in standard padded base64, as RFC 7617 and Google's Go and Python libraries do. Upstream sends URL-safe base64 without padding.
- A service account's key, an authorized user's `token_uri` and `quota_project_id`, the file's `token_url` and impersonation URL, a URL source's URL and headers, and `GCE_METADATA_HOST` are parsed when the credentials are loaded, so a bad one fails when the registry is set up rather than at the first token. An AWS source's `region_url` and `imdsv2_session_token_url` are parsed when the region is looked up, which is also at setup, and not at all when the region comes from elsewhere. A URL that is not `http` or `https` is refused.
- An answer that is not UTF-8 is an error. Upstream replaces the invalid bytes of a URL subject token.
- An `expires_in` past what the clock can hold is an error, rather than a token that never expires.
- An error shows no request URL, as its query can hold a token.
- An answer over 1 MiB is an error, and an error answer is read no further than it is quoted, so a misbehaving endpoint cannot exhaust memory.
- The AWS subject token signs the STS URL's port when it names one other than the scheme's.
- A Local Zone or Wavelength Zone from `region_url`, such as `us-west-2-lax-1a`, gives its parent region, where upstream takes the zone's name for a region that does not exist.
- A credential source that names `environment_id` must parse as an AWS source, where upstream tries the next source, so a malformed AWS source is an error rather than read as the URL source it also names.
- An error keeps at most the first 200 bytes of an answer's body, and a missing subject token field is not shown with the document it was looked for in, as either can hold a token.
- The `x-goog-api-client` telemetry header is not sent.

Not supported, each failing with an error that names it: the executable subject token source, `gdch_service_account` and any other credential type not listed above. ID tokens, the signer, API keys and credential access boundaries are not used by `ocync` and are not built.

Upstream tests not ported, and why:

- `token_cache.rs`: `ocync` caches the token in its registry provider's `CredentialCache` and refreshes it before expiry on use, without upstream's background task.
- `retry.rs` and `errors.rs`: the retry is the crate's own, and the classification is tested on each call's failures.
- Each type's caching tests (`header_caching`, `token_caching`, `creds_from_json_custom_uri_with_caching`): the same cache serves every type, and is tested once.
- The AWS key and role URL tests (`test_resolve_credentials_env`, `_imds`, `test_subject_token_imdsv2_success`): `ocync` takes AWS credentials from its own chain, whose tests cover them.
- The metadata server's error-message, email and endpoint-listing tests (`error_message_with_adc`, `adc_no_mds`, `validate_default_endpoint_urls`): `ocync` asks the server only for a token, and its own tests cover the endpoint and failures.
- The quota project tests, including `GOOGLE_CLOUD_QUOTA_PROJECT`: the quota project goes only in upstream's request headers for Google APIs, which `ocync` does not call. An authorized user's `quota_project_id` is still forwarded to IAM when that user is an impersonation source, as upstream forwards its source's headers.
- The `idtoken`, `gdch`, `signer`, access boundary, API key, executable source and universe-domain lookup tests: none of that is built.

## Owned AWS request signing

`aws-sigv4`, the SDK's signer, computes Signature Version 4 with the pure-Rust `sha2` and `hmac` crates, outside the FIPS module, so `ocync` makes its AWS calls itself, in `crates/ocync-distribution/src/aws/`: ECR and ECR Public (`GetAuthorizationToken`, `BatchCheckLayerAvailability`), and the credential chain's STS, IAM Identity Center and SSO OIDC calls.

- An owned signer on `aws-lc-rs` HMAC and SHA-256. It signs the query too, sorted and encoded again as AWS canonicalizes it, for the `GetCallerIdentity` request Google's federation from AWS sends. It passes the cases of the AWS signing test suite that apply to header signing outside S3, query cases included, and matched `aws-sigv4` on 60,000 random requests without a query.
- Endpoints from the AWS SDK's endpoint rules for every partition, for ECR, ECR Public, STS, IAM Identity Center and SSO OIDC, with `AWS_USE_FIPS_ENDPOINT`, `AWS_USE_DUALSTACK_ENDPOINT`, `AWS_ENDPOINT_URL` and each service's `AWS_ENDPOINT_URL_<SERVICE>` read from the AWS config. A query or user and password in a configured endpoint URL is dropped with a warning, as the SDK sends neither, and the URL is never logged. A configured URL's path is kept, and the operation's path is joined to it with one slash, as the SDK joins them.
- The SDK's retry policy, ported from `aws-smithy-runtime` with its test cases and read from the same settings: `AWS_MAX_ATTEMPTS` (3 by default), `AWS_RETRY_MODE` and `AWS_NEW_RETRIES_2026`. Throttling error codes (ECR throttles with HTTP 400) are retried as throttles; timeout codes, HTTP 500, 502, 503 and 504, and connection, timeout and I/O failures as transient; other errors are not retried. Each retry waits a jittered exponential backoff from 1 second, at most 20 seconds, or what `x-amz-retry-after` asks, and spends from a quota of 500 shared by every call to one service in one region: 5 for a throttle or skewed signature and 10 for a transient failure. Each call holds one retry permit, the cost of its latest retry. Its success returns that cost, or 1 when it holds none, and with too little left the call stops retrying. The SDK holds one permit per client instead, so a retry on one call replaces another call's permit and leaves that cost spent. An `AWS_MAX_ATTEMPTS` or `AWS_RETRY_MODE` that does not parse is a configuration error, where the SDK panics. Retry behavior 2.1 starts other failures at 50 ms, costs 14 for them and bounds `x-amz-retry-after` to 5 seconds past the backoff. Adaptive mode paces sends with the SDK's CUBIC rate limiter once a throttle is seen. One deliberate difference: an HTTP 429 without a throttling code is retried as a throttle, since a proxy on `HTTPS_PROXY` sends one and a batch check given up on transfers every blob it covered. A call that receives nothing for 20 seconds after it is sent, or between reads, fails. The SDK applies that limit only to a response body once it is streaming, so this is stricter.
- The SDK's clock-skew correction: the skew is measured from each answer's `Date` header, and a signature rejected while the clocks are more than 4 minutes apart is signed again on AWS's clock. `AWS_DISABLE_CLOCK_SKEW_CORRECTION=true`, or the profile's `disable_clock_skew_correction`, turns it off.
- A credential source that does not answer within 21.6 seconds, the SDK's bound for its default settings, fails the call.
- Requests honor `HTTPS_PROXY` and `NO_PROXY`, like registry traffic, where the SDK's client does not.

## Owned AWS credential chain

`ocync` resolves AWS credentials itself, in `crates/ocync-distribution/src/aws/credentials/`, as `aws-config` 1.13.0 does. Every case of its `default-credential-provider-chain` and `profile-provider` test data runs against the owned chain, with its recorded HTTP traffic. The sources, in AWS precedence order, as [Amazon ECR](/registries/ecr) documents them:

- Environment variables.
- Shared config and credentials files, including `source_profile` and `role_arn` role chaining, `credential_source`, and the per-registry `aws_profile` field.
- Web identity (IRSA) through `AssumeRoleWithWebIdentity`.
- `credential_process`.
- IAM Identity Center (`aws sso login`): the cached token is exchanged with `GetRoleCredentials`, and refreshed with `CreateToken` within 5 minutes of expiry, at most once every 30 seconds, then written back to the shared cache in a form the AWS CLI still reads. Neither call is signed. The cache file name is a SHA-1 of the session name, computed with `aws-lc-rs`. Loads of one token in a process take turns, so it is refreshed once.
- The container credential endpoint (ECS task roles, EKS Pod Identity).
- The EC2 instance metadata service, IMDSv2 only.
- FIPS endpoints through `AWS_USE_FIPS_ENDPOINT`, and the GovCloud, China and ISO partitions.

Credentials are loaded per profile and region rather than per client, and reloaded shortly before they expire. Within a run they are shared by the clients that need them, such as a registry's auth provider and its batch checker. Each run, including each watch cycle, starts afresh, so it reads the shared files again and drops credentials AWS may have revoked. A source that is not configured passes to the next, and any other failure stops the chain, as upstream. STS, IAM Identity Center and SSO OIDC calls honor `HTTPS_PROXY` and `NO_PROXY`. The instance metadata service and the container endpoint never use a proxy, so their tokens reach no proxy host.

Deliberate differences from upstream:

- Container endpoint: a plain HTTP host name is pinned to the addresses checked, so a second DNS answer cannot redirect the token. An empty DNS answer is refused, and so is a scheme other than `http` or `https`. The endpoint has its own retry quota.
- The account ID a source reports is not kept: nothing in `ocync` uses it.
- A `login_session` profile is refused.
- `credential_process`: a command the chain gives up on is killed, so a hung helper does not outlive the timeout. An expiry with a leap second (`:60`) is refused. A failing command's stderr is cut to its first 200 bytes in the error, as it can hold a token and the error reaches the report.
- `credential_process` and the JSON credential endpoints: when a key appears twice in different cases, the one that sorts last is read, where upstream reads the one written last.
- IAM Identity Center write-back: fields the refresh does not set are kept, and only the fields it sets are rewritten. The file is written to a new file only its owner can read, then renamed over the old one. A write that fails is logged as a warning and the refreshed token is still used, where upstream fails the load. The new expiry is written to the second.
- IAM Identity Center refresh: an answer with no token counts as a failed refresh, which keeps the cached token. A failed refresh is logged as a warning, where upstream logs it at debug level. A refresh is given up after 5 seconds and the cached token used, so a refresh that hangs cannot use up the 21.6 second bound and leave no token; the attempt is counted when it starts.
- Instance metadata: when a refresh fails, the last credentials are served with a passed expiry pushed out 10 to 15 minutes, as fresh ones are, so the client does not reload them on every call while the service is down. Upstream serves them with the expiry they were stored with.
- The cached token is read from disk on every load, so a token another process refreshed is picked up.
- The profile resolver refuses a profile that names only a login token as it reads the profile, where upstream refuses it when it builds the provider. The upstream profile row that checks this is not run; an owned test checks the same outcome.

## Alternatives considered

Lighter crates were evaluated before choosing to own the code:

| Crate | Why it does not fit |
|---|---|
| `reqsign` (Apache OpenDAL) | The core crate depends on the pure-Rust `sha1`, `sha2` and `hmac` crates, so signing stays outside the FIPS module. |
| `aws-sign-v4` | Built on `ring`, which `deny.toml` bans. |
| `aws-creds` | Credentials only, blocking HTTP, and no IAM Identity Center, `source_profile` role chaining or `credential_process`. |
| `gcp_auth` | Its cryptography runs on `aws-lc-rs`, but it has no Workload Identity Federation. Federation lets `ocync` outside Google Cloud reach Artifact Registry without a long-lived service account key. |

## Replacing a dependency

Each replacement starts from the upstream crate's tests and works backwards:

1. Copy the upstream tests and test data into the repo with Apache-2.0 attribution and the upstream commit recorded. Point them at the new module's API, so they fail.
2. Implement until they pass. The AWS signing test suite shipped in `aws-sigv4` (`aws-signing-test-suite/v4`) is the reference for the signer. `google-cloud-auth` and `aws-config` supply theirs.
3. An upstream test for behavior `ocync` deliberately does not support is listed in this document with the reason. A gap is a decision, never an omission.
4. While both implementations exist, the upstream crate stays as a dev-dependency and a differential test runs both on the same inputs: identical signatures, identical resolved credentials.
5. Before the upstream crate is removed, the real binary runs against real ECR, ECR Public and Artifact Registry, and the result is diffed against the base commit.
