# Typed reactive Docker contract

Agent Hub accepts only workflow documents with `workflow.schema = "reactive_v2"`.
States and transitions are typed, executable states use Docker, and every executable
state declares `repo_access`. Network access requires an explicit typed
`network_resources` request; omission means no network authority.

Execution is released only from an immutable resolved authorization request after
the provider-authoritative checkpoint is committed. General command and agent jobs
do not receive host GitHub configuration or broad GitHub credentials. Reviewed
managed GitHub actions bind one exact host, repository, branch/base, operation,
access class, and host-specific credential provider.

This is a pre-product destructive migration. Startup removes incompatible transient
orchestration state transactionally. Provider checkpoints remain authoritative for
recovery. Unverifiable active side effects are quarantined by returning provider
ownership to the remembered human; inconsistent ownership trees fail startup.

There is no workflow-schema or executor compatibility path. Superseded workflow
documents, host execution, aliases, and fallbacks are rejected or deleted.
