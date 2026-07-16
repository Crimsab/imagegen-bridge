# Operations runbook

This runbook covers a normal private deployment. Keep deployment-specific
addresses, credentials, and orchestration overrides outside the public
repository.

## Routine health check

```sh
curl --fail http://127.0.0.1:8787/health/live
curl --fail http://127.0.0.1:8787/health/ready
docker compose exec imagegen-bridge imagegen-bridge \
  --config /config/imagegen-bridge.toml doctor --non-interactive --json
docker compose logs --tail 100 imagegen-bridge
```

Liveness proves that the process can serve requests. Readiness uses cached,
detail-free provider state. Authenticated diagnostics contain bounded,
redaction-safe operational details.

## Upgrade

1. Read the release notes and retain the current image tag.
2. Take a SQLite-consistent state backup.
3. Run `config check` with the new binary or image.
4. Pull or build the new release without deleting volumes.
5. Replace the container and wait for readiness.
6. Verify authentication, capabilities, one synchronous generation, one edit
   when used, streaming, and durable job completion.
7. Confirm that existing jobs, presets, sessions, and artifacts remain visible.

Do not consider an upgrade complete based on liveness alone.

## Back up

Stop the service or use a SQLite-aware snapshot for `/data/state`. Copying only
the main database file while WAL writes are active is not a valid backup.

Back up these classes separately:

| Data | Sensitivity | Restore requirement |
| --- | --- | --- |
| Codex home | Secret | Encrypted storage and correct UID ownership |
| State databases | Private | SQLite-consistent snapshot |
| Artifacts | Potentially private | Preserve filenames and checksums |
| Configuration | Non-secret by design | Preserve version and deployment overrides |
| Bridge bearer | Secret | Secret manager or protected local file |

Test restoration on an isolated listener before relying on the backup.

## Roll back

1. Stop the failed release.
2. Preserve its logs and state before changing anything.
3. Restore the prior image and configuration together.
4. Reuse the existing volumes unless the new release performed an incompatible
   migration documented in its release notes.
5. If storage must be restored, use the last verified consistent snapshot.
6. Verify readiness and the same smoke matrix used for upgrades.

Never delete or hand-edit SQLite files as an automatic repair.

## Rotate the API bearer

Changing the bearer intentionally creates a new durable-history ownership
scope. Existing jobs become inaccessible to the new bearer rather than being
silently reassigned.

Plan rotation as a client migration:

1. Finish or cancel active jobs.
2. Record any history that must remain accessible.
3. replace the secret through the deployment's secret mechanism;
4. restart the bridge;
5. update clients and verify that the old bearer receives `401`.

## Refresh Codex OAuth

Run `codex login` outside the container, update only the dedicated Codex home,
preserve UID `10001` ownership, and restart. Do not print `auth.json`, place it
in an environment variable, or mount a complete user home while debugging.

## Diagnose readiness failures

Work from least invasive to most invasive:

1. Check authenticated diagnostics and `doctor --non-interactive --json`.
2. Verify that the dedicated Codex home exists and is writable by UID `10001`.
3. Verify provider configuration and the Codex executable for app-server mode.
4. Inspect bounded recent logs for stable error codes.
5. Re-authenticate outside the container only when diagnostics indicate an
   authentication problem.

Avoid repeated live probes. They consume image allowance and can obscure the
original failure.
