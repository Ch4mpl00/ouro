`features/` is the Conformance Test Kit of the Open Workflow DSL (formerly
Serverless Workflow), copied unmodified from
https://github.com/serverlessworkflow/specification (`ctk/features`, commit
3aa1687ce4ab38f1d2dbede500b3077c4f801b66, the same as `../../schema`),
Copyright the Serverless Workflow Specification authors, Apache-2.0
(https://www.apache.org/licenses/LICENSE-2.0). Update it together with the
schema and `../spec-examples`.

`../ctk.rs` runs it with cucumber: `cargo test -p ows --test ctk`. The
scenarios that call HTTP and OpenAPI services reach local fakes through
`http` and `openapi` host functions, as a protocol pack would; nothing goes
over the network. The kit's error filter names the communication error type
`https://open-workflow-specification.org/dsl/errors/types/communication`,
which `WorkflowError::has_type` accepts for the standard type.
