# Secrets

`src/secret/` stores secret values (LUKS passphrases, Ceph/iSCSI auth, TLS keys)
associated with a UUID and usage. These are sensitive by definition.

## Core invariants

- Secret values are confidential: don't log them, don't include them in error
  messages or debug output, and zero/free them promptly (`virSecureErase` /
  careful free) rather than leaving copies around.
- ACL: reading a secret value (`virSecretGetValue`) is a privileged operation
  with its own ACL check; a new path that exposes secret data must enforce it.
- The on-disk store (under the secrets state dir) must have restrictive
  permissions; a new file written with the value needs the correct mode and
  atomic write.
- Lookup is by UUID or by usage (type + usage-id); validate the client-supplied
  key and handle "not found" without leaking whether a secret exists where that
  matters.

## Common findings

- Secret value logged, returned in an error message, or left unzeroed after use.
- Missing ACL check on a new value-exposing operation.
- State file written with overly broad permissions or non-atomically.
- Unchecked lookup return (NULL secret) dereferenced.
