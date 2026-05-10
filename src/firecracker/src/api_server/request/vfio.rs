// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use micro_http::Body;
use vmm::rpc_interface::VmmAction;
use vmm::vmm_config::vfio::VfioDeviceConfig;

use super::super::parsed_request::{ParsedRequest, RequestError};

pub(crate) fn parse_put_vfio_device(body: &Body) -> Result<ParsedRequest, RequestError> {
    Ok(ParsedRequest::new_sync(VmmAction::SetVfioDevice(
        serde_json::from_slice::<VfioDeviceConfig>(body.raw())?,
    )))
}
