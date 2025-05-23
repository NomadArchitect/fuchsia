// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use bt_test_harness::host::{self as host, HostHarness};
use fidl_fuchsia_bluetooth_sys as sys;
use fuchsia_bluetooth::expectation::asynchronous::ExpectableExt;
use fuchsia_bluetooth::expectation::{self};
use fuchsia_bluetooth::types::{Address, BondingData, LeBondData, OneOrBoth, PeerId};

// TODO(https://fxbug.dev/42072596): Add tests for BR/EDR and dual mode bond data.

fn new_le_bond_data(id: &PeerId, address: &Address, name: &str, has_ltk: bool) -> BondingData {
    BondingData {
        identifier: (*id).into(),
        local_address: Address::Public([0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA]),
        address: (*address).clone(),
        name: Some(name.to_string()),
        data: OneOrBoth::Left(LeBondData {
            connection_parameters: None,
            services: vec![],
            local_ltk: None,
            peer_ltk: if has_ltk {
                Some(sys::Ltk {
                    key: sys::PeerKey {
                        security: sys::SecurityProperties {
                            authenticated: true,
                            secure_connections: false,
                            encryption_key_size: 16,
                        },
                        data: sys::Key {
                            value: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
                        },
                    },
                    ediv: 1,
                    rand: 2,
                })
            } else {
                None
            },
            irk: None,
            csrk: None,
        }),
    }
}

async fn restore_bonds(
    state: &HostHarness,
    bonds: Vec<BondingData>,
) -> Result<Vec<BondingData>, Error> {
    let bonds: Vec<_> = bonds.into_iter().map(sys::BondingData::from).collect();
    let fut = state.aux().bonding_delegate.restore_bonds(&bonds);
    let errors = fut.await?;
    Ok(errors.into_iter().map(BondingData::try_from).collect::<Result<Vec<_>, _>>()?)
}

const TEST_ID1: PeerId = PeerId(0x1234);
const TEST_ID2: PeerId = PeerId(0x5678);
const TEST_ADDR1: Address = Address::Public([6, 5, 4, 3, 2, 1]);
const TEST_ADDR2: Address = Address::Public([1, 2, 3, 4, 5, 6]);
const TEST_NAME1: &str = "Name1";
const TEST_NAME2: &str = "Name2";

#[test_harness::run_singlethreaded_test(
    test_component = "fuchsia-pkg://fuchsia.com/bt-bonding-integration-tests#meta/bt-bonding-integration-tests-component.cm"
)]
async fn test_restore_no_bonds_succeeds(harness: HostHarness) {
    let errors = restore_bonds(&harness, vec![]).await.unwrap();
    assert_eq!(errors, vec![]);
}

// Tests initializing bonded LE devices.
#[test_harness::run_singlethreaded_test(
    test_component = "fuchsia-pkg://fuchsia.com/bt-bonding-integration-tests#meta/bt-bonding-integration-tests-component.cm"
)]
async fn test_restore_bonded_devices_success(harness: HostHarness) {
    // Peers should be initially empty.
    assert_eq!(harness.write_state().peers().len(), 0);

    let bond_data1 = new_le_bond_data(&TEST_ID1, &TEST_ADDR1, TEST_NAME1, true /* has LTK */);
    let bond_data2 = new_le_bond_data(&TEST_ID2, &TEST_ADDR2, TEST_NAME2, true /* has LTK */);
    let errors = restore_bonds(&harness, vec![bond_data1, bond_data2]).await.unwrap();
    assert_eq!(errors, vec![]);

    // We should receive notifications for the newly added devices.
    let expected1 = expectation::peer::address(TEST_ADDR1)
        .and(expectation::peer::technology(fidl_fuchsia_bluetooth_sys::TechnologyType::LowEnergy))
        .and(expectation::peer::name(TEST_NAME1))
        .and(expectation::peer::bonded(true));

    let expected2 = expectation::peer::address(TEST_ADDR2)
        .and(expectation::peer::technology(fidl_fuchsia_bluetooth_sys::TechnologyType::LowEnergy))
        .and(expectation::peer::name(TEST_NAME2))
        .and(expectation::peer::bonded(true));

    let _ = host::expectation::peer(&harness, expected1).await.unwrap();
    let _ = host::expectation::peer(&harness, expected2).await.unwrap();
}

#[test_harness::run_singlethreaded_test(
    test_component = "fuchsia-pkg://fuchsia.com/bt-bonding-integration-tests#meta/bt-bonding-integration-tests-component.cm"
)]
async fn test_restore_bonded_devices_no_ltk_fails(harness: HostHarness) {
    // Peers should be initially empty.
    assert_eq!(harness.write_state().peers().len(), 0);

    // Inserting a bonded device without a LTK should fail.
    let bond_data = new_le_bond_data(&TEST_ID1, &TEST_ADDR1, TEST_NAME1, false /* no LTK */);
    let errors = restore_bonds(&harness, vec![bond_data.clone()]).await.unwrap();
    assert_eq!(errors, vec![bond_data]);
    assert_eq!(harness.write_state().peers().len(), 0);
}

#[test_harness::run_singlethreaded_test(
    test_component = "fuchsia-pkg://fuchsia.com/bt-bonding-integration-tests#meta/bt-bonding-integration-tests-component.cm"
)]
async fn test_restore_bonded_devices_duplicate_entry(harness: HostHarness) {
    // Peers should be initially empty.
    assert_eq!(harness.write_state().peers().len(), 0);

    // Initialize one entry.
    let bond_data = new_le_bond_data(&TEST_ID1, &TEST_ADDR1, TEST_NAME1, true /* with LTK */);
    let errors = restore_bonds(&harness, vec![bond_data]).await.unwrap();
    assert_eq!(errors, vec![]);

    // We should receive a notification for the newly added device.
    let expected = expectation::peer::address(TEST_ADDR1)
        .and(expectation::peer::technology(fidl_fuchsia_bluetooth_sys::TechnologyType::LowEnergy))
        .and(expectation::peer::bonded(true));
    let _ = host::expectation::peer(&harness, expected.clone()).await.unwrap();

    // Adding an entry with the existing id should fail.
    let bond_data = new_le_bond_data(&TEST_ID1, &TEST_ADDR2, TEST_NAME2, true /* with LTK */);
    let errors = restore_bonds(&harness, vec![bond_data.clone()]).await.unwrap();
    assert_eq!(errors, vec![bond_data]);

    // Adding an entry with a different ID but existing address should fail.
    let bond_data = new_le_bond_data(&TEST_ID2, &TEST_ADDR1, TEST_NAME1, true /* with LTK */);
    let errors = restore_bonds(&harness, vec![bond_data.clone()]).await.unwrap();
    assert_eq!(errors, vec![bond_data]);
}

// Tests that adding a list of bonding data with malformed content succeeds for the valid entries
// but reports an error.
#[test_harness::run_singlethreaded_test(
    test_component = "fuchsia-pkg://fuchsia.com/bt-bonding-integration-tests#meta/bt-bonding-integration-tests-component.cm"
)]
async fn test_restore_bonded_devices_invalid_entry(harness: HostHarness) {
    // Peers should be initially empty.
    assert_eq!(harness.write_state().peers().len(), 0);

    // Add one entry with no LTK (invalid) and one with (valid). This should create an entry for the
    // valid device but report an error for the invalid entry.
    let no_ltk = new_le_bond_data(&TEST_ID1, &TEST_ADDR1, TEST_NAME1, false);
    let with_ltk = new_le_bond_data(&TEST_ID2, &TEST_ADDR2, TEST_NAME2, true);
    let errors = restore_bonds(&harness, vec![no_ltk.clone(), with_ltk]).await.unwrap();
    assert_eq!(errors, vec![no_ltk]);

    let expected = expectation::peer::address(TEST_ADDR2)
        .and(expectation::peer::technology(fidl_fuchsia_bluetooth_sys::TechnologyType::LowEnergy))
        .and(expectation::peer::bonded(true));
    let _ = host::expectation::peer(&harness, expected.clone()).await.unwrap();
}
