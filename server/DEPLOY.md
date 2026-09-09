# agent-hub-server deployment

Run the orchestration server with:

```text
agent-hub-server serve
```

Provide the authoritative config root, workflow catalog, task state directory,
orchestration database, artifact root, API authentication, and optional gRPC
listener described in `README.md`.

The approval service is deployed independently. Set
`AGENT_HUB_APPROVALS_SERVER` and, when required,
`AGENT_HUB_APPROVALS_TOKEN` to connect orchestration jobs to it through
`agent-hub-approval-client`. Do not route approval paths through this server;
`agent-hub-server` intentionally has no approval, question, session-notification,
or MCP endpoints.
