use common::block_on;
use common::privkey::key_pair_from_seed;
use crate::WithdrawFee;
use crate::utxo::rpc_clients::{ElectrumProtocol};
use futures::future::join_all;
use mocktopus::mocking::*;
use super::*;

fn electrum_client_for_test(servers: &[&str]) -> UtxoRpcClientEnum {
    let mut client = ElectrumClientImpl::new();
    for server in servers {
        client.add_server(&ElectrumRpcRequest {
            url: server.to_string(),
            protocol: ElectrumProtocol::TCP,
            disable_cert_verification: false,
        }).unwrap();
    }

    let mut attempts = 0;
    while !block_on(client.is_connected()) {
        if attempts >= 10 {
            panic!("Failed to connect to at least 1 of {:?} in 5 seconds.", servers);
        }

        thread::sleep(Duration::from_millis(500));
        attempts += 1;
    }

    UtxoRpcClientEnum::Electrum(ElectrumClient(Arc::new(client)))
}

fn utxo_coin_for_test(rpc_client: UtxoRpcClientEnum, force_seed: Option<&str>) -> UtxoCoin {
    let checksum_type = ChecksumType::DSHA256;
    let default_seed = "spice describe gravity federal blast come thank unfair canal monkey style afraid";
    let seed = match force_seed {
        Some(s) => s.into(),
        None => match std::env::var("BOB_PASSPHRASE") {
            Ok(p) => if p.is_empty() {
                default_seed.into()
            } else {
                p
            },
            Err(_) => default_seed.into(),
        }
    };
    let key_pair = key_pair_from_seed(&seed).unwrap();
    let my_address = Address {
        prefix: 60,
        hash: key_pair.public().address_hash(),
        t_addr_prefix: 0,
        checksum_type,
    };

    let coin = UtxoCoinImpl {
        decimals: 8,
        rpc_client,
        key_pair,
        is_pos: false,
        notarized: false,
        overwintered: true,
        segwit: false,
        tx_version: 4,
        my_address,
        asset_chain: true,
        p2sh_addr_prefix: 85,
        p2sh_t_addr_prefix: 0,
        pub_addr_prefix: 60,
        pub_t_addr_prefix: 0,
        ticker: "ETOMIC".into(),
        wif_prefix: 0,
        tx_fee: TxFee::Fixed(1000),
        version_group_id: 0x892f2085,
        zcash: true,
        checksum_type,
        fork_id: 0,
        signature_version: SignatureVersion::Base,
        history_sync_state: Mutex::new(HistorySyncState::NotEnabled),
        required_confirmations: 1.into(),
    };

    UtxoCoin(Arc::new(coin))
}

#[test]
fn test_extract_secret() {
    let tx: UtxoTx = "0100000001de7aa8d29524906b2b54ee2e0281f3607f75662cbc9080df81d1047b78e21dbc00000000d7473044022079b6c50820040b1fbbe9251ced32ab334d33830f6f8d0bf0a40c7f1336b67d5b0220142ccf723ddabb34e542ed65c395abc1fbf5b6c3e730396f15d25c49b668a1a401209da937e5609680cb30bff4a7661364ca1d1851c2506fa80c443f00a3d3bf7365004c6b6304f62b0e5cb175210270e75970bb20029b3879ec76c4acd320a8d0589e003636264d01a7d566504bfbac6782012088a9142fb610d856c19fd57f2d0cffe8dff689074b3d8a882103f368228456c940ac113e53dad5c104cf209f2f102a409207269383b6ab9b03deac68ffffffff01d0dc9800000000001976a9146d9d2b554d768232320587df75c4338ecc8bf37d88ac40280e5c".into();
    let secret = tx.extract_secret().unwrap();
    let expected_secret = hex::decode("9da937e5609680cb30bff4a7661364ca1d1851c2506fa80c443f00a3d3bf7365").unwrap();
    assert_eq!(expected_secret, secret);
}

#[test]
fn test_generate_transaction() {
    let client = electrum_client_for_test(&["test1.cipig.net:10025"]);
    let coin = utxo_coin_for_test(client, None);
    let unspents = vec![UnspentInfo {
        value: 10000000000,
        outpoint: OutPoint::default(),
    }];

    let outputs = vec![TransactionOutput {
        script_pubkey: vec![].into(),
        value: 999,
    }];

    let generated = coin.generate_transaction(unspents, outputs, FeePolicy::SendExact, None).wait();
    // must not allow to use output with value < dust
    unwrap_err!(generated);

    let unspents = vec![UnspentInfo {
        value: 100000,
        outpoint: OutPoint::default(),
    }];

    let outputs = vec![TransactionOutput {
        script_pubkey: vec![].into(),
        value: 98001,
    }];

    let generated = unwrap!(coin.generate_transaction(unspents, outputs, FeePolicy::SendExact, None).wait());
    // the change that is less than dust must be included to miner fee
    // so no extra outputs should appear in generated transaction
    assert_eq!(generated.0.outputs.len(), 1);

    assert_eq!(generated.1.fee_amount, 1999);
    assert_eq!(generated.1.received_by_me, 0);
    assert_eq!(generated.1.spent_by_me, 100000);

    let unspents = vec![UnspentInfo {
        value: 100000,
        outpoint: OutPoint::default(),
    }];

    let outputs = vec![TransactionOutput {
        script_pubkey: Builder::build_p2pkh(&coin.my_address.hash).to_bytes(),
        value: 100000,
    }];

    // test that fee is properly deducted from output amount equal to input amount (max withdraw case)
    let generated = unwrap!(coin.generate_transaction(unspents, outputs, FeePolicy::DeductFromOutput(0), None).wait());
    assert_eq!(generated.0.outputs.len(), 1);

    assert_eq!(generated.1.fee_amount, 1000);
    assert_eq!(generated.1.received_by_me, 99000);
    assert_eq!(generated.1.spent_by_me, 100000);
    assert_eq!(generated.0.outputs[0].value, 99000);

    let unspents = vec![UnspentInfo {
        value: 100000,
        outpoint: OutPoint::default(),
    }];

    let outputs = vec![TransactionOutput {
        script_pubkey: vec![].into(),
        value: 100000,
    }];

    // test that generate_transaction returns an error when input amount is not sufficient to cover output + fee
    unwrap_err!(coin.generate_transaction(unspents, outputs, FeePolicy::SendExact, None).wait());
}

#[test]
fn test_addresses_from_script() {
    let client = electrum_client_for_test(&["test1.cipig.net:10025"]);
    let coin = utxo_coin_for_test(client, None);
    // P2PKH
    let script: Script = "76a91405aab5342166f8594baf17a7d9bef5d56744332788ac".into();
    let expected_addr: Vec<Address> = vec!["R9o9xTocqr6CeEDGDH6mEYpwLoMz6jNjMW".into()];
    let actual_addr = unwrap!(coin.addresses_from_script(&script));
    assert_eq!(expected_addr, actual_addr);

    // P2SH
    let script: Script = "a914e71a6120653ebd526e0f9d7a29cde5969db362d487".into();
    let expected_addr: Vec<Address> = vec!["bZoEPR7DjTqSDiQTeRFNDJuQPTRY2335LD".into()];
    let actual_addr = unwrap!(coin.addresses_from_script(&script));
    assert_eq!(expected_addr, actual_addr);
}

#[test]
fn test_kmd_interest() {
    let value = 64605500822;
    let lock_time = 1556623906;
    let current_time = 1556623906 + 3600 + 300;
    let expected = 36870;
    let actual = kmd_interest(1000001, value, lock_time, current_time);
    assert_eq!(expected, actual);

    // UTXO amount must be at least 10 KMD to be eligible for interest
    let value = 999999999;
    let lock_time = 1556623906;
    let current_time = 1556623906 + 3600 + 300;
    let expected = 0;
    let actual = kmd_interest(1000001, value, lock_time, current_time);
    assert_eq!(expected, actual);
}

#[test]
fn test_sat_from_big_decimal() {
    let amount = "0.000001".parse().unwrap();
    let sat = sat_from_big_decimal(&amount, 18).unwrap();
    let expected_sat = 1000000000000;
    assert_eq!(expected_sat, sat);

    let amount = "0.12345678".parse().unwrap();
    let sat = sat_from_big_decimal(&amount, 8).unwrap();
    let expected_sat = 12345678;
    assert_eq!(expected_sat, sat);

    let amount = "1.000001".parse().unwrap();
    let sat = sat_from_big_decimal(&amount, 18).unwrap();
    let expected_sat = 1000001000000000000;
    assert_eq!(expected_sat, sat);

    let amount = 1.into();
    let sat = sat_from_big_decimal(&amount, 18).unwrap();
    let expected_sat = 1000000000000000000;
    assert_eq!(expected_sat, sat);

    let amount = "0.000000000000000001".parse().unwrap();
    let sat = sat_from_big_decimal(&amount, 18).unwrap();
    let expected_sat = 1u64;
    assert_eq!(expected_sat, sat);

    let amount = 1234.into();
    let sat = sat_from_big_decimal(&amount, 9).unwrap();
    let expected_sat = 1234000000000;
    assert_eq!(expected_sat, sat);

    let amount = 1234.into();
    let sat = sat_from_big_decimal(&amount, 0).unwrap();
    let expected_sat = 1234;
    assert_eq!(expected_sat, sat);

    let amount = 1234.into();
    let sat = sat_from_big_decimal(&amount, 1).unwrap();
    let expected_sat = 12340;
    assert_eq!(expected_sat, sat);

    let amount = "1234.12345".parse().unwrap();
    let sat = sat_from_big_decimal(&amount, 1).unwrap();
    let expected_sat = 12341;
    assert_eq!(expected_sat, sat);
}

#[test]
fn test_wait_for_payment_spend_timeout_native() {
    let client = NativeClientImpl {
        uri: "http://127.0.0.1:10271".to_owned(),
        auth: fomat!("Basic " (base64_encode("user481805103:pass97a61c8d048bcf468c6c39a314970e557f57afd1d8a5edee917fb29bafb3a43371", URL_SAFE))),
    };

    static mut OUTPUT_SPEND_CALLED: bool = false;
    NativeClient::find_output_spend.mock_safe(|_, _, _, _| {
        unsafe { OUTPUT_SPEND_CALLED = true };
        MockResult::Return(Ok(None))
    });
    let client = UtxoRpcClientEnum::Native(NativeClient(Arc::new(client)));
    let coin = utxo_coin_for_test(client, None);
    let transaction = unwrap!(hex::decode("01000000000102fff7f7881a8099afa6940d42d1e7f6362bec38171ea3edf433541db4e4ad969f00000000494830450221008b9d1dc26ba6a9cb62127b02742fa9d754cd3bebf337f7a55d114c8e5cdd30be022040529b194ba3f9281a99f2b1c0a19c0489bc22ede944ccf4ecbab4cc618ef3ed01eeffffffef51e1b804cc89d182d279655c3aa89e815b1b309fe287d9b2b55d57b90ec68a0100000000ffffffff02202cb206000000001976a9148280b37df378db99f66f85c95a783a76ac7a6d5988ac9093510d000000001976a9143bde42dbee7e4dbe6a21b2d50ce2f0167faa815988ac000247304402203609e17b84f6a7d30c80bfa610b5b4542f32a8a0d5447a12fb1366d7f01cc44a0220573a954c4518331561406f90300e8f3358f51928d43c212a8caed02de67eebee0121025476c2e83188368da1ff3e292e7acafcdb3566bb0ad253f62fc70f07aeee635711000000"));
    let wait_until = now_ms() / 1000 - 1;
    let from_block = 1000;

    assert!(coin.wait_for_tx_spend(&transaction, wait_until, from_block).is_err());
    assert!(unsafe { OUTPUT_SPEND_CALLED });
}

#[test]
fn test_wait_for_payment_spend_timeout_electrum() {
    static mut OUTPUT_SPEND_CALLED: bool = false;
    ElectrumClient::find_output_spend.mock_safe(|_, _, _, _| {
        unsafe { OUTPUT_SPEND_CALLED = true };
        MockResult::Return(Ok(None))
    });

    let client = ElectrumClientImpl::new();
    let client = UtxoRpcClientEnum::Electrum(ElectrumClient(Arc::new(client)));
    let coin = utxo_coin_for_test(client, None);
    let transaction = unwrap!(hex::decode("01000000000102fff7f7881a8099afa6940d42d1e7f6362bec38171ea3edf433541db4e4ad969f00000000494830450221008b9d1dc26ba6a9cb62127b02742fa9d754cd3bebf337f7a55d114c8e5cdd30be022040529b194ba3f9281a99f2b1c0a19c0489bc22ede944ccf4ecbab4cc618ef3ed01eeffffffef51e1b804cc89d182d279655c3aa89e815b1b309fe287d9b2b55d57b90ec68a0100000000ffffffff02202cb206000000001976a9148280b37df378db99f66f85c95a783a76ac7a6d5988ac9093510d000000001976a9143bde42dbee7e4dbe6a21b2d50ce2f0167faa815988ac000247304402203609e17b84f6a7d30c80bfa610b5b4542f32a8a0d5447a12fb1366d7f01cc44a0220573a954c4518331561406f90300e8f3358f51928d43c212a8caed02de67eebee0121025476c2e83188368da1ff3e292e7acafcdb3566bb0ad253f62fc70f07aeee635711000000"));
    let wait_until = now_ms() / 1000 - 1;
    let from_block = 1000;

    assert!(coin.wait_for_tx_spend(&transaction, wait_until, from_block).is_err());
    assert!(unsafe { OUTPUT_SPEND_CALLED });
}

#[test]
fn test_search_for_swap_tx_spend_electrum_was_spent() {
    let client = electrum_client_for_test(&["test1.cipig.net:10025", "test2.cipig.net:10025", "test3.cipig.net:10025"]);
    let coin = utxo_coin_for_test(client, Some("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
    // raw tx bytes of https://etomic.explorer.dexstats.info/tx/c514b3163d66636ebc3574817cb5853d5ab39886183de71ffedf5c5768570a6b
    let payment_tx_bytes = unwrap!(hex::decode("0400008085202f89013ac014d4926c8b435f7a5c58f38975d14f1aba597b1eef2dfdc093457678eb83010000006a47304402204ddb9b10237a1267a02426d923528213ad1e0b62d45be7d9629e2909f099d90c02205eecadecf6fd09cb8465170eb878c5d54e563f067b64e23c418da0f6519ca354012102031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3ffffffff02809698000000000017a914bbd726b74f27b476d5d932e903b5893fd4e8bd2187acdaaa87010000001976a91405aab5342166f8594baf17a7d9bef5d56744332788ac2771515d000000000000000000000000000000"));

    // raw tx bytes of https://etomic.explorer.dexstats.info/tx/e72be40bab15f3914e70507e863e26b8ccfaa75a9861d6fe706b39cab1272617
    let spend_tx_bytes = unwrap!(hex::decode("0400008085202f89016b0a5768575cdffe1fe73d188698b35a3d85b57c817435bc6e63663d16b314c500000000d8483045022100d3cf75d26d977c0358e46c5db3753aa332ba12130b36b24b541cb90416f4606102201d805c5bdfc4d630cb78adb63239911c97a09c41125af37be219e876610f15f201209ac4a742e81a47dc26a3ddf83de84783fdb49c2322c4a3fdafc0613bf3335c40004c6b6304218f515db1752102031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3ac6782012088a914954f5a3f3b5de4410e5e1a82949410de95a4b6ba882102631dcf1d4b1b693aa8c2751afc68e4794b1e5996566cfc701a663f8b7bbbe640ac68ffffffff0198929800000000001976a91464ae8510aac9546d5e7704e31ce177451386455588ac3963515d000000000000000000000000000000"));
    let spend_tx = TransactionEnum::UtxoTx(unwrap!(deserialize(spend_tx_bytes.as_slice())));

    let found = unwrap!(unwrap!(coin.search_for_swap_tx_spend(
        1565626145,
        coin.key_pair.public(),
        &unwrap!(Public::from_slice(&unwrap!(hex::decode("02631dcf1d4b1b693aa8c2751afc68e4794b1e5996566cfc701a663f8b7bbbe640")))),
        &unwrap!(hex::decode("954f5a3f3b5de4410e5e1a82949410de95a4b6ba")),
        &payment_tx_bytes,
        0
    )));
    assert_eq!(FoundSwapTxSpend::Spent(spend_tx), found);
}

#[test]
fn test_search_for_swap_tx_spend_electrum_was_refunded() {
    let client = electrum_client_for_test(&["test1.cipig.net:10025", "test2.cipig.net:10025", "test3.cipig.net:10025"]);
    let coin = utxo_coin_for_test(client, Some("spice describe gravity federal blast come thank unfair canal monkey style afraid"));

    // raw tx bytes of https://etomic.explorer.dexstats.info/tx/c9a47cc6e80a98355cd4e69d436eae6783cbee5991756caa6e64a0743442fa96
    let payment_tx_bytes = unwrap!(hex::decode("0400008085202f8901887e809b10738b1625b7f47fd5d2201f32e8a4c6c0aaefc3b9ab6c07dc6a5925010000006a47304402203966f49ba8acc9fcc0e53e7b917ca5599ce6054a0c2d22752c57a3dc1b0fc83502206fde12c869da20a21cedd5bbc4bcd12977d25ff4b00e0999de5ac4254668e891012102031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3ffffffff02809698000000000017a9147e9456f37fa53cf9053e192ea4951d2c8b58647c8784631684010000001976a91405aab5342166f8594baf17a7d9bef5d56744332788ac0fb3525d000000000000000000000000000000"));

    // raw tx bytes of https://etomic.explorer.dexstats.info/tx/a5f11bdb657ee834a4c410e2001beccce0374bfa3f662bd890fd3d01b0b3d101
    let spend_tx_bytes = unwrap!(hex::decode("0400008085202f890196fa423474a0646eaa6c759159eecb8367ae6e439de6d45c35980ae8c67ca4c900000000c4483045022100969c3b2c1ab630b67a6ee74316c9356e38872276a070d126da1d731503bb6e3e02204398d8c23cb59c2caddc33ce9c9716c54b11574126a3c3350a17043f3751696d01514c786304cf93525db1752102031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3ac6782012088a92102031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3882102031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3ac68feffffff0198929800000000001976a91405aab5342166f8594baf17a7d9bef5d56744332788ac01a5525d000000000000000000000000000000"));
    let spend_tx = TransactionEnum::UtxoTx(unwrap!(deserialize(spend_tx_bytes.as_slice())));

    let found = unwrap!(unwrap!(coin.search_for_swap_tx_spend(
        1565692879,
        coin.key_pair.public(),
        &unwrap!(Public::from_slice(&unwrap!(hex::decode("02031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3")))),
        &unwrap!(hex::decode("02031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3")),
        &payment_tx_bytes,
        0
    )));
    assert_eq!(FoundSwapTxSpend::Refunded(spend_tx), found);
}

#[test]
fn test_withdraw_impl_set_fixed_fee() {
    NativeClient::list_unspent_ordered.mock_safe(|_,_| {
        let unspents = vec![UnspentInfo { outpoint: OutPoint { hash: 1.into(), index: 0 }, value: 1000000000 }];
        MockResult::Return(Box::new(futures01::future::ok(unspents)))
    });

    let client = NativeClient(Arc::new(NativeClientImpl {
        uri: "http://127.0.0.1".to_owned(),
        auth: fomat!("Basic " (base64_encode("user481805103:pass97a61c8d048bcf468c6c39a314970e557f57afd1d8a5edee917fb29bafb3a43371", URL_SAFE))),
    }));

    let coin = utxo_coin_for_test(UtxoRpcClientEnum::Native(client), None);

    let withdraw_req = WithdrawRequest {
        amount: 1.into(),
        to: "RQq6fWoy8aGGMLjvRfMY5mBNVm2RQxJyLa".to_string(),
        coin: "ETOMIC".to_string(),
        max: false,
        fee: Some(WithdrawFee::UtxoFixed { amount: "0.1".parse().unwrap() }),
    };
    let expected = Some(UtxoFeeDetails {
        amount: "0.1".parse().unwrap()
    }.into());
    let tx_details = unwrap!(block_on(withdraw_impl(coin.clone(), withdraw_req)));
    assert_eq!(expected, tx_details.fee_details);
}

#[test]
fn test_withdraw_impl_sat_per_kb_fee() {
    NativeClient::list_unspent_ordered.mock_safe(|_,_| {
        let unspents = vec![UnspentInfo { outpoint: OutPoint { hash: 1.into(), index: 0 }, value: 1000000000 }];
        MockResult::Return(Box::new(futures01::future::ok(unspents)))
    });

    let client = NativeClient(Arc::new(NativeClientImpl {
        uri: "http://127.0.0.1".to_owned(),
        auth: fomat!("Basic " (base64_encode("user481805103:pass97a61c8d048bcf468c6c39a314970e557f57afd1d8a5edee917fb29bafb3a43371", URL_SAFE))),
    }));

    let coin = utxo_coin_for_test(UtxoRpcClientEnum::Native(client), None);

    let withdraw_req = WithdrawRequest {
        amount: 1.into(),
        to: "RQq6fWoy8aGGMLjvRfMY5mBNVm2RQxJyLa".to_string(),
        coin: "ETOMIC".to_string(),
        max: false,
        fee: Some(WithdrawFee::UtxoPerKbyte { amount: "0.1".parse().unwrap() }),
    };
    // The resulting transaction size might be 244 or 245 bytes depending on signature size
    // MM2 always expects the worst case during fee calculation
    // 0.1 * 245 / 1024 ~ 0.02392577
    let expected = Some(UtxoFeeDetails {
        amount: "0.02392577".parse().unwrap()
    }.into());
    let tx_details = unwrap!(block_on(withdraw_impl(coin.clone(), withdraw_req)));
    assert_eq!(expected, tx_details.fee_details);
}

#[test]
fn test_withdraw_impl_sat_per_kb_fee_amount_equal_to_max() {
    NativeClient::list_unspent_ordered.mock_safe(|_,_| {
        let unspents = vec![UnspentInfo { outpoint: OutPoint { hash: 1.into(), index: 0 }, value: 1000000000 }];
        MockResult::Return(Box::new(futures01::future::ok(unspents)))
    });

    let client = NativeClient(Arc::new(NativeClientImpl {
        uri: "http://127.0.0.1".to_owned(),
        auth: fomat!("Basic " (base64_encode("user481805103:pass97a61c8d048bcf468c6c39a314970e557f57afd1d8a5edee917fb29bafb3a43371", URL_SAFE))),
    }));

    let coin = utxo_coin_for_test(UtxoRpcClientEnum::Native(client), None);

    let withdraw_req = WithdrawRequest {
        amount: "9.97939454".parse().unwrap(),
        to: "RQq6fWoy8aGGMLjvRfMY5mBNVm2RQxJyLa".to_string(),
        coin: "ETOMIC".to_string(),
        max: false,
        fee: Some(WithdrawFee::UtxoPerKbyte { amount: "0.1".parse().unwrap() }),
    };
    let tx_details = unwrap!(block_on(withdraw_impl(coin.clone(), withdraw_req)));
    // The resulting transaction size might be 210 or 211 bytes depending on signature size
    // MM2 always expects the worst case during fee calculation
    // 0.1 * 211 / 1024 ~ 0.02060546
    let expected_fee = Some(UtxoFeeDetails {
        amount: "0.02060546".parse().unwrap()
    }.into());
    assert_eq!(expected_fee, tx_details.fee_details);
    let expected_balance_change = BigDecimal::from(-10);
    assert_eq!(expected_balance_change, tx_details.my_balance_change);
}

#[test]
fn test_withdraw_impl_sat_per_kb_fee_amount_equal_to_max_dust_included_to_fee() {
    NativeClient::list_unspent_ordered.mock_safe(|_,_| {
        let unspents = vec![UnspentInfo { outpoint: OutPoint { hash: 1.into(), index: 0 }, value: 1000000000 }];
        MockResult::Return(Box::new(futures01::future::ok(unspents)))
    });

    let client = NativeClient(Arc::new(NativeClientImpl {
        uri: "http://127.0.0.1".to_owned(),
        auth: fomat!("Basic " (base64_encode("user481805103:pass97a61c8d048bcf468c6c39a314970e557f57afd1d8a5edee917fb29bafb3a43371", URL_SAFE))),
    }));

    let coin = utxo_coin_for_test(UtxoRpcClientEnum::Native(client), None);

    let withdraw_req = WithdrawRequest {
        amount: "9.97939454".parse().unwrap(),
        to: "RQq6fWoy8aGGMLjvRfMY5mBNVm2RQxJyLa".to_string(),
        coin: "ETOMIC".to_string(),
        max: false,
        fee: Some(WithdrawFee::UtxoPerKbyte { amount: "0.09999999".parse().unwrap() }),
    };
    let tx_details = unwrap!(block_on(withdraw_impl(coin.clone(), withdraw_req)));
    // The resulting transaction size might be 210 or 211 bytes depending on signature size
    // MM2 always expects the worst case during fee calculation
    // 0.1 * 211 / 1024 ~ 0.02060546
    let expected_fee = Some(UtxoFeeDetails {
        amount: "0.02060546".parse().unwrap()
    }.into());
    assert_eq!(expected_fee, tx_details.fee_details);
    let expected_balance_change = BigDecimal::from(-10);
    assert_eq!(expected_balance_change, tx_details.my_balance_change);
}

#[test]
fn test_withdraw_impl_sat_per_kb_fee_amount_over_max() {
    NativeClient::list_unspent_ordered.mock_safe(|_,_| {
        let unspents = vec![UnspentInfo { outpoint: OutPoint { hash: 1.into(), index: 0 }, value: 1000000000 }];
        MockResult::Return(Box::new(futures01::future::ok(unspents)))
    });

    let client = NativeClient(Arc::new(NativeClientImpl {
        uri: "http://127.0.0.1".to_owned(),
        auth: fomat!("Basic " (base64_encode("user481805103:pass97a61c8d048bcf468c6c39a314970e557f57afd1d8a5edee917fb29bafb3a43371", URL_SAFE))),
    }));

    let coin = utxo_coin_for_test(UtxoRpcClientEnum::Native(client), None);

    let withdraw_req = WithdrawRequest {
        amount: "9.97939455".parse().unwrap(),
        to: "RQq6fWoy8aGGMLjvRfMY5mBNVm2RQxJyLa".to_string(),
        coin: "ETOMIC".to_string(),
        max: false,
        fee: Some(WithdrawFee::UtxoPerKbyte { amount: "0.1".parse().unwrap() }),
    };
    unwrap_err!(block_on(withdraw_impl(coin.clone(), withdraw_req)));
}

#[test]
fn test_withdraw_impl_sat_per_kb_fee_max() {
    NativeClient::list_unspent_ordered.mock_safe(|_,_| {
        let unspents = vec![UnspentInfo { outpoint: OutPoint { hash: 1.into(), index: 0 }, value: 1000000000 }];
        MockResult::Return(Box::new(futures01::future::ok(unspents)))
    });

    let client = NativeClient(Arc::new(NativeClientImpl {
        uri: "http://127.0.0.1".to_owned(),
        auth: fomat!("Basic " (base64_encode("user481805103:pass97a61c8d048bcf468c6c39a314970e557f57afd1d8a5edee917fb29bafb3a43371", URL_SAFE))),
    }));

    let coin = utxo_coin_for_test(UtxoRpcClientEnum::Native(client), None);

    let withdraw_req = WithdrawRequest {
        amount: 0.into(),
        to: "RQq6fWoy8aGGMLjvRfMY5mBNVm2RQxJyLa".to_string(),
        coin: "ETOMIC".to_string(),
        max: true,
        fee: Some(WithdrawFee::UtxoPerKbyte { amount: "0.1".parse().unwrap() }),
    };
    // The resulting transaction size might be 210 or 211 bytes depending on signature size
    // MM2 always expects the worst case during fee calculation
    // 0.1 * 211 / 1024 ~ 0.02060546
    let expected = Some(UtxoFeeDetails {
        amount: "0.02060546".parse().unwrap()
    }.into());
    let tx_details = unwrap!(block_on(withdraw_impl(coin.clone(), withdraw_req)));
    assert_eq!(expected, tx_details.fee_details);
}

#[test]
fn test_utxo_lock() {
    // send several transactions concurrently to check that they are not using same inputs
    let client = electrum_client_for_test(&["test1.cipig.net:10025", "test2.cipig.net:10025", "test3.cipig.net:10025"]);
    let coin = utxo_coin_for_test(client, None);
    let output = TransactionOutput {
        value: 1000000,
        script_pubkey: Builder::build_p2pkh(&coin.my_address.hash).to_bytes(),
    };
    let mut futures = vec![];
    for _ in 0..5 {
        futures.push(send_outputs_from_my_address_impl(coin.clone(), vec![output.clone()]));
    }
    let results = block_on(join_all(futures));
    for result in results {
        unwrap!(result);
    }
}
