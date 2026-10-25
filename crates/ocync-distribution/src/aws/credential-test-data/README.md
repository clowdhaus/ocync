# AWS credential chain test data

Copied from [`awslabs/aws-sdk-rust`](https://github.com/awslabs/aws-sdk-rust)
`sdk/aws-config/test-data`, as published with `aws-config` 1.13.0 (commit
`b964fc77de60b0a492ff788c40bb66524fd26a44`). Licensed under the Apache License,
Version 2.0. Copyright Amazon.com, Inc. or its affiliates.

Each case directory holds the process environment (`env.json`), the files the
chain may read (`fs/`), the recorded HTTP exchanges (`http-traffic.json`) and
the expected credentials or error text (`test-case.json`). The cases run
through `credentials::fixtures` in this crate, unchanged.

`imds-config/imds-endpoint-tests.json` holds the instance metadata endpoint
cases, run by the tests of `credentials::imds`.

`ecs-tests.json` holds the container endpoint cases, run by the tests of
`credentials::container`.
