## On-disk cache versions

The SQLite index cache uses schema major **3**. A cache with a different schema
major is disposable and is rebuilt from vault files. Rename journal manifests
use version **2**; an older or corrupt manifest is quarantined during open and
reported through an action-required notice. These version changes do not alter
the note files that are the vault's source of truth.
