//! Gateway integration test suite
//!
//! This crate contains integration tests for the gateway API
//! and business logic.
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use assert_matches::assert_matches;
use bitcoin::Network;
use bitcoin_hashes::{sha256, Hash};
use fedimint_client::transaction::{ClientInput, ClientOutput, TransactionBuilder};
use fedimint_client::ClientHandleArc;
use fedimint_core::config::FederationId;
use fedimint_core::core::{IntoDynInstance, OperationId};
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::task::sleep_in_test;
use fedimint_core::util::NextOrPending;
use fedimint_core::{msats, sats, Amount, OutPoint, TransactionId};
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_common::config::DummyGenParams;
use fedimint_dummy_server::DummyInit;
use fedimint_ln_client::api::LnFederationApi;
use fedimint_ln_client::pay::{PayInvoicePayload, PaymentData};
use fedimint_ln_client::{
    LightningClientInit, LightningClientModule, LightningClientStateMachines,
    LightningOperationMeta, LightningOperationMetaVariant, LnPayState, LnReceiveState,
    MockGatewayConnection, OutgoingLightningPayment, PayType,
};
use fedimint_ln_common::config::{FeeToAmount, GatewayFee, LightningGenParams};
use fedimint_ln_common::contracts::incoming::IncomingContractOffer;
use fedimint_ln_common::contracts::outgoing::OutgoingContractAccount;
use fedimint_ln_common::contracts::{EncryptedPreimage, FundedContract, Preimage, PreimageKey};
use fedimint_ln_common::{LightningGateway, LightningInput, LightningOutput, PrunedInvoice};
use fedimint_ln_server::LightningInit;
use fedimint_logging::LOG_TEST;
use fedimint_testing::btc::BitcoinTest;
use fedimint_testing::db::BYTE_33;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use fedimint_testing::gateway::{GatewayTest, DEFAULT_GATEWAY_PASSWORD};
use fedimint_testing::ln::FakeLightningTest;
use fedimint_unknown_common::config::UnknownGenParams;
use fedimint_unknown_server::UnknownInit;
use futures::Future;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description, RoutingFees};
use ln_gateway::rpc::rpc_client::{GatewayRpcClient, GatewayRpcError, GatewayRpcResult};
use ln_gateway::rpc::rpc_server::hash_password;
use ln_gateway::rpc::{
    BalancePayload, ConnectFedPayload, FederationRoutingFees, LeaveFedPayload,
    SetConfigurationPayload,
};
use ln_gateway::state_machine::pay::{
    OutgoingContractError, OutgoingPaymentError, OutgoingPaymentErrorType,
};
use ln_gateway::state_machine::{
    GatewayClientModule, GatewayClientStateMachines, GatewayExtPayStates, GatewayExtReceiveStates,
    GatewayMeta, Htlc,
};
use ln_gateway::{DEFAULT_FEES, DEFAULT_NETWORK};
use reqwest::StatusCode;
use tracing::info;

async fn user_pay_invoice(
    ln_module: &LightningClientModule,
    invoice: Bolt11Invoice,
    gateway_id: &PublicKey,
) -> anyhow::Result<OutgoingLightningPayment> {
    let gateway = ln_module.select_gateway(gateway_id).await;
    ln_module.pay_bolt11_invoice(gateway, invoice, ()).await
}

fn fixtures() -> Fixtures {
    info!(target: LOG_TEST, "Setting up fixtures");
    let fixtures = Fixtures::new_primary(DummyClientInit, DummyInit, DummyGenParams::default())
        .with_server_only_module(UnknownInit, UnknownGenParams::default());
    let ln_params = LightningGenParams::regtest(fixtures.bitcoin_server());
    fixtures.with_module(
        LightningClientInit {
            gateway_conn: Arc::new(MockGatewayConnection),
        },
        LightningInit,
        ln_params,
    )
}

async fn single_federation_test<B>(
    f: impl FnOnce(
            GatewayTest,
            FakeLightningTest,
            FederationTest,
            ClientHandleArc, // User Client
            Arc<dyn BitcoinTest>,
        ) -> B
        + Copy,
) -> anyhow::Result<()>
where
    B: Future<Output = anyhow::Result<()>>,
{
    let fixtures = fixtures();
    let other_ln = FakeLightningTest::new();

    let fed = fixtures.new_default_fed().await;
    let mut gateway = fixtures
        .new_gateway(0, Some(DEFAULT_GATEWAY_PASSWORD.to_string()))
        .await;
    gateway.connect_fed(&fed).await;
    let user_client = fed.new_client().await;
    let bitcoin = fixtures.bitcoin();
    f(gateway, other_ln, fed, user_client, bitcoin).await?;

    Ok(())
}

async fn multi_federation_test<B>(
    f: impl FnOnce(
            GatewayTest,
            GatewayRpcClient,
            FederationTest,
            FederationTest,
            Arc<dyn BitcoinTest>,
        ) -> B
        + Copy,
) -> anyhow::Result<()>
where
    B: Future<Output = anyhow::Result<()>>,
{
    let fixtures = fixtures();
    let fed1 = fixtures.new_default_fed().await;
    let fed2 = fixtures.new_default_fed().await;

    let gateway = fixtures
        .new_gateway(0, Some(DEFAULT_GATEWAY_PASSWORD.to_string()))
        .await;
    let client = gateway
        .get_rpc()
        .with_password(Some(DEFAULT_GATEWAY_PASSWORD.to_string()));

    f(gateway, client, fed1, fed2, fixtures.bitcoin()).await?;
    Ok(())
}

fn sha256(data: &[u8]) -> sha256::Hash {
    sha256::Hash::hash(data)
}

/// Helper function for constructing the `PaymentData` that the gateway uses to
/// pay the invoice. LND supports "private" payments where the description is
/// stripped from the invoice.
fn get_payment_data(gateway: Option<LightningGateway>, invoice: Bolt11Invoice) -> PaymentData {
    match gateway {
        Some(g) if g.supports_private_payments => {
            let pruned_invoice: PrunedInvoice = invoice.try_into().expect("Invoice has amount");
            PaymentData::PrunedInvoice(pruned_invoice)
        }
        _ => PaymentData::Invoice(invoice),
    }
}

/// Test helper function for paying a valid BOLT11 invoice with a gateway
/// specified by `gateway_id`.
async fn gateway_pay_valid_invoice(
    invoice: Bolt11Invoice,
    user_client: &ClientHandleArc,
    gateway_client: &ClientHandleArc,
    gateway_id: &PublicKey,
) -> anyhow::Result<()> {
    let user_lightning_module = &user_client.get_first_module::<LightningClientModule>();
    let gateway = user_lightning_module.select_gateway(gateway_id).await;

    // User client pays test invoice
    let OutgoingLightningPayment {
        payment_type,
        contract_id,
        fee: _,
    } = user_pay_invoice(user_lightning_module, invoice.clone(), gateway_id).await?;
    match payment_type {
        PayType::Lightning(pay_op) => {
            let mut pay_sub = user_lightning_module
                .subscribe_ln_pay(pay_op)
                .await?
                .into_stream();
            assert_eq!(pay_sub.ok().await?, LnPayState::Created);
            let funded = pay_sub.ok().await?;
            assert_matches!(funded, LnPayState::Funded { .. });

            let payload = PayInvoicePayload {
                federation_id: user_client.federation_id(),
                contract_id,
                payment_data: get_payment_data(gateway, invoice),
                preimage_auth: Hash::hash(&[0; 32]),
            };

            let gw_pay_op = gateway_client
                .get_first_module::<GatewayClientModule>()
                .gateway_pay_bolt11_invoice(payload)
                .await?;
            let mut gw_pay_sub = gateway_client
                .get_first_module::<GatewayClientModule>()
                .gateway_subscribe_ln_pay(gw_pay_op)
                .await?
                .into_stream();
            assert_eq!(gw_pay_sub.ok().await?, GatewayExtPayStates::Created);
            assert_matches!(gw_pay_sub.ok().await?, GatewayExtPayStates::Preimage { .. });

            let dummy_module = gateway_client.get_first_module::<DummyClientModule>();
            if let GatewayExtPayStates::Success { out_points, .. } = gw_pay_sub.ok().await? {
                for outpoint in out_points {
                    dummy_module.receive_money(outpoint).await?;
                }
            } else {
                panic!("Gateway pay state machine was not successful");
            }
        }
        _ => panic!("Expected Lightning payment!"),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_client_pay_valid_invoice() -> anyhow::Result<()> {
    single_federation_test(
        |gateway, other_lightning_client, fed, user_client, _| async move {
            let gateway_client = gateway.select_client(fed.id()).await;
            // Print money for user_client
            let dummy_module = user_client.get_first_module::<DummyClientModule>();
            let (_, outpoint) = dummy_module.print_money(sats(1000)).await?;
            dummy_module.receive_money(outpoint).await?;
            assert_eq!(user_client.get_balance().await, sats(1000));

            // Create test invoice
            let invoice = other_lightning_client.invoice(sats(250), None)?;

            gateway_pay_valid_invoice(
                invoice,
                &user_client,
                &gateway_client,
                &gateway.gateway.gateway_id,
            )
            .await?;

            assert_eq!(user_client.get_balance().await, sats(1000 - 250));
            assert_eq!(gateway_client.get_balance().await, sats(250));

            Ok(())
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_can_change_default_routing_fees() -> anyhow::Result<()> {
    single_federation_test(
        |gateway, other_lightning_client, fed, user_client, _| async move {
            let rpc_client = gateway
                .get_rpc()
                .with_password(Some(DEFAULT_GATEWAY_PASSWORD.to_string()));
            // Print money for user_client
            let dummy_module = user_client.get_first_module::<DummyClientModule>();
            let (_, outpoint) = dummy_module.print_money(sats(1000)).await?;
            dummy_module.receive_money(outpoint).await?;
            assert_eq!(user_client.get_balance().await, sats(1000));

            let fee = "10,10000".to_string();
            let federation_fee = FederationRoutingFees::from_str(&fee)?;
            let set_configuration_payload = SetConfigurationPayload {
                password: None,
                num_route_hints: None,
                routing_fees: Some(federation_fee.clone()),
                network: None,
                per_federation_routing_fees: None,
            };
            verify_gateway_rpc_success("set_configuration", || {
                rpc_client.set_configuration(set_configuration_payload.clone())
            })
            .await;

            // we need to reconnect to set the fees as defaults from gateway
            reconnect_federation(&rpc_client, &fed).await;

            // Update the gateway cache since the fees have changed
            let ln_module = user_client.get_first_module::<LightningClientModule>();
            ln_module.update_gateway_cache().await?;

            // Create test invoice
            let invoice_amount = sats(250);
            let invoice = other_lightning_client.invoice(invoice_amount, None)?;

            let gateway_client = gateway.select_client(fed.id()).await;
            gateway_pay_valid_invoice(
                invoice,
                &user_client,
                &gateway_client,
                &gateway.gateway.gateway_id,
            )
            .await?;

            let fee: RoutingFees = federation_fee.into();
            let fee_amount = fee.to_amount(&invoice_amount);
            assert_eq!(
                user_client.get_balance().await,
                sats(1000 - 250) - fee_amount
            );
            assert_eq!(gateway_client.get_balance().await, sats(250) + fee_amount);

            Ok(())
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_can_change_federation_routing_fees() -> anyhow::Result<()> {
    single_federation_test(
        |gateway, other_lightning_client, fed, user_client, _| async move {
            let rpc_client = gateway
                .get_rpc()
                .with_password(Some(DEFAULT_GATEWAY_PASSWORD.to_string()));
            // Print money for user_client
            let dummy_module = user_client.get_first_module::<DummyClientModule>();
            let (_, outpoint) = dummy_module.print_money(sats(1000)).await?;
            dummy_module.receive_money(outpoint).await?;
            assert_eq!(user_client.get_balance().await, sats(1000));

            let fee = "10,10000".to_string();
            let federation_fee = FederationRoutingFees::from_str(&fee)?;
            let set_configuration_payload = SetConfigurationPayload {
                password: None,
                num_route_hints: None,
                routing_fees: None,
                network: None,
                per_federation_routing_fees: Some(vec![(fed.id(), federation_fee.clone())]),
            };
            verify_gateway_rpc_success("set_configuration", || {
                rpc_client.set_configuration(set_configuration_payload.clone())
            })
            .await;

            // Update the gateway cache since the fees have changed
            let ln_module = user_client.get_first_module::<LightningClientModule>();
            ln_module.update_gateway_cache().await?;

            // Create test invoice
            let invoice_amount = sats(250);
            let invoice = other_lightning_client.invoice(invoice_amount, None)?;

            let gateway_client = gateway.select_client(fed.id()).await;
            gateway_pay_valid_invoice(
                invoice,
                &user_client,
                &gateway_client,
                &gateway.gateway.gateway_id,
            )
            .await?;

            let fee: RoutingFees = federation_fee.into();
            let fee_amount = fee.to_amount(&invoice_amount);
            assert_eq!(
                user_client.get_balance().await,
                sats(1000 - 250) - fee_amount
            );
            assert_eq!(gateway_client.get_balance().await, sats(250) + fee_amount);

            Ok(())
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_enforces_fees() -> anyhow::Result<()> {
    single_federation_test(
        |gateway_test, other_lightning_client, fed, user_client, _| async move {
            let rpc_client = gateway_test
                .get_rpc()
                .with_password(Some(DEFAULT_GATEWAY_PASSWORD.to_string()));
            // Print money for user_client
            let dummy_module = user_client.get_first_module::<DummyClientModule>();
            let (_, outpoint) = dummy_module.print_money(sats(1000)).await?;
            dummy_module.receive_money(outpoint).await?;
            assert_eq!(user_client.get_balance().await, sats(1000));

            // Change the fees of the gateway
            let fee = "10,10000".to_string();
            let federation_fee = FederationRoutingFees::from_str(&fee)?;
            let set_configuration_payload = SetConfigurationPayload {
                password: None,
                num_route_hints: None,
                routing_fees: Some(federation_fee),
                network: None,
                per_federation_routing_fees: None,
            };
            verify_gateway_rpc_success("set_configuration", || {
                rpc_client.set_configuration(set_configuration_payload.clone())
            })
            .await;

            // we need to reconnect to set the fees as defaults from gateway
            reconnect_federation(&rpc_client, &fed).await;

            info!("### Changed gateway routing fees");

            let user_lightning_module = user_client.get_first_module::<LightningClientModule>();
            let gateway_id = gateway_test.gateway.gateway_id;
            let gateway = user_lightning_module.select_gateway(&gateway_id).await;
            let gateway_client = gateway_test.select_client(fed.id()).await;

            let invoice_amount = sats(250);
            let invoice = other_lightning_client.invoice(invoice_amount, None)?;

            // Try to pay an invoice, this should fail since the client will not set the
            // gateway's fees.
            info!("### User client paying invoice");
            let OutgoingLightningPayment {
                payment_type,
                contract_id,
                fee: _,
            } = user_lightning_module
                .pay_bolt11_invoice(gateway.clone(), invoice.clone(), ())
                .await
                .expect("No Lightning Payment was started");
            match payment_type {
                PayType::Lightning(pay_op) => {
                    let mut pay_sub = user_lightning_module
                        .subscribe_ln_pay(pay_op)
                        .await?
                        .into_stream();
                    assert_eq!(pay_sub.ok().await?, LnPayState::Created);
                    let funded = pay_sub.ok().await?;
                    assert_matches!(funded, LnPayState::Funded { .. });
                    info!("### User client funded contract");

                    let payload = PayInvoicePayload {
                        federation_id: user_client.federation_id(),
                        contract_id,
                        payment_data: get_payment_data(gateway, invoice),
                        preimage_auth: Hash::hash(&[0; 32]),
                    };

                    let gw_pay_op = gateway_client
                        .get_first_module::<GatewayClientModule>()
                        .gateway_pay_bolt11_invoice(payload)
                        .await?;
                    let mut gw_pay_sub = gateway_client
                        .get_first_module::<GatewayClientModule>()
                        .gateway_subscribe_ln_pay(gw_pay_op)
                        .await?
                        .into_stream();
                    assert_eq!(gw_pay_sub.ok().await?, GatewayExtPayStates::Created);
                    info!("### Gateway client started payment");
                    assert_matches!(
                        gw_pay_sub.ok().await?,
                        GatewayExtPayStates::Canceled {
                            error: OutgoingPaymentError {
                                error_type: OutgoingPaymentErrorType::InvalidOutgoingContract {
                                    error: OutgoingContractError::Underfunded(_, _)
                                },
                                ..
                            }
                        }
                    );
                    info!("### Gateway client canceled payment");
                }
                _ => panic!("Expected Lightning payment!"),
            }

            Ok(())
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_cannot_claim_invalid_preimage() -> anyhow::Result<()> {
    single_federation_test(
        |gateway, other_lightning_client, fed, user_client, _| async move {
            let gateway_id = gateway.gateway.gateway_id;
            let gateway_client = gateway.select_client(fed.id()).await;
            // Print money for user_client
            let dummy_module = user_client.get_first_module::<DummyClientModule>();
            let (_, outpoint) = dummy_module.print_money(sats(1000)).await?;
            dummy_module.receive_money(outpoint).await?;
            assert_eq!(user_client.get_balance().await, sats(1000));

            // Fund outgoing contract that the user client expects the gateway to pay
            let invoice = other_lightning_client.invoice(sats(250), None)?;
            let OutgoingLightningPayment {
                payment_type: _,
                contract_id,
                fee: _,
            } = user_pay_invoice(
                &user_client.get_first_module(),
                invoice.clone(),
                &gateway_id,
            )
            .await?;

            // Try to directly claim the outgoing contract with an invalid preimage
            let gateway_module = gateway_client.get_first_module::<GatewayClientModule>();

            let account = gateway_module.api.wait_contract(contract_id).await?;
            let outgoing_contract = match account.contract {
                FundedContract::Outgoing(contract) => OutgoingContractAccount {
                    amount: account.amount,
                    contract,
                },
                _ => {
                    panic!("Expected OutgoingContract");
                }
            };

            // Bogus preimage
            let preimage = Preimage(rand::random());
            let claim_input = outgoing_contract.claim(preimage);
            let client_input = ClientInput::<LightningInput, GatewayClientStateMachines> {
                input: claim_input,
                state_machines: Arc::new(|_, _| vec![]),
                amount: outgoing_contract.amount,
                keys: vec![gateway_module.redeem_key],
            };

            let tx = TransactionBuilder::new().with_input(client_input.into_dyn(gateway_module.id));
            let operation_meta_gen = |_: TransactionId, _: Vec<OutPoint>| GatewayMeta::Pay {};
            let operation_id = OperationId(invoice.payment_hash().to_byte_array());
            let (txid, _) = gateway_client
                .finalize_and_submit_transaction(
                    operation_id,
                    fedimint_ln_common::KIND.as_str(),
                    operation_meta_gen,
                    tx,
                )
                .await?;

            // Assert that we did not get paid for claiming a contract with a bogus preimage
            assert!(dummy_module
                .receive_money(OutPoint { txid, out_idx: 0 })
                .await
                .is_err());
            assert_eq!(gateway_client.get_balance().await, sats(0));
            Ok(())
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_client_pay_unpayable_invoice() -> anyhow::Result<()> {
    single_federation_test(
        |gateway, other_lightning_client, fed, user_client, _| async move {
            let gateway_id = gateway.gateway.gateway_id;
            let gateway_client = gateway.select_client(fed.id()).await;
            // Print money for user client
            let dummy_module = user_client.get_first_module::<DummyClientModule>();
            let lightning_module = user_client.get_first_module::<LightningClientModule>();
            let (_, outpoint) = dummy_module.print_money(sats(1000)).await?;
            dummy_module.receive_money(outpoint).await?;
            assert_eq!(user_client.get_balance().await, sats(1000));

            // Create invoice that cannot be paid
            let invoice = other_lightning_client.unpayable_invoice(sats(250), None);

            let gateway = lightning_module.select_gateway(&gateway_id).await;

            // User client pays test invoice
            let OutgoingLightningPayment {
                payment_type,
                contract_id,
                fee: _,
            } = user_pay_invoice(&lightning_module, invoice.clone(), &gateway_id).await?;
            match payment_type {
                PayType::Lightning(pay_op) => {
                    let mut pay_sub = lightning_module
                        .subscribe_ln_pay(pay_op)
                        .await?
                        .into_stream();
                    assert_eq!(pay_sub.ok().await?, LnPayState::Created);
                    let funded = pay_sub.ok().await?;
                    assert_matches!(funded, LnPayState::Funded { .. });

                    let payload = PayInvoicePayload {
                        federation_id: user_client.federation_id(),
                        contract_id,
                        payment_data: get_payment_data(gateway, invoice),
                        preimage_auth: Hash::hash(&[0; 32]),
                    };

                    let gw_pay_op = gateway_client
                        .get_first_module::<GatewayClientModule>()
                        .gateway_pay_bolt11_invoice(payload)
                        .await?;
                    let mut gw_pay_sub = gateway_client
                        .get_first_module::<GatewayClientModule>()
                        .gateway_subscribe_ln_pay(gw_pay_op)
                        .await?
                        .into_stream();
                    assert_eq!(gw_pay_sub.ok().await?, GatewayExtPayStates::Created);
                    assert_matches!(gw_pay_sub.ok().await?, GatewayExtPayStates::Canceled { .. });
                }
                _ => panic!("Expected Lightning payment!"),
            }

            Ok(())
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_client_intercept_valid_htlc() -> anyhow::Result<()> {
    single_federation_test(|gateway, _, fed, user_client, _| async move {
        let gateway_id = gateway.gateway.gateway_id;
        let gateway_client = gateway.select_client(fed.id()).await;
        // Print money for gateway client
        let initial_gateway_balance = sats(1000);
        let dummy_module = gateway_client.get_first_module::<DummyClientModule>();
        let (_, outpoint) = dummy_module.print_money(initial_gateway_balance).await?;
        dummy_module.receive_money(outpoint).await?;
        assert_eq!(gateway_client.get_balance().await, sats(1000));

        // User client creates invoice in federation
        let invoice_amount = sats(100);
        let ln_module = user_client.get_first_module::<LightningClientModule>();
        let ln_gateway = ln_module.select_gateway(&gateway_id).await;
        let desc = Description::new("description".to_string())?;
        let (_invoice_op, invoice, _) = ln_module
            .create_bolt11_invoice(
                invoice_amount,
                Bolt11InvoiceDescription::Direct(&desc),
                None,
                "test intercept valid HTLC",
                ln_gateway,
            )
            .await?;

        // Run gateway state machine
        let htlc = Htlc {
            payment_hash: *invoice.payment_hash(),
            incoming_amount_msat: Amount::from_msats(invoice.amount_milli_satoshis().unwrap()),
            outgoing_amount_msat: Amount::from_msats(invoice.amount_milli_satoshis().unwrap()),
            incoming_expiry: u32::MAX,
            short_channel_id: Some(1),
            incoming_chan_id: 2,
            htlc_id: 1,
        };
        let intercept_op = gateway_client
            .get_first_module::<GatewayClientModule>()
            .gateway_handle_intercepted_htlc(htlc)
            .await?;
        let mut intercept_sub = gateway_client
            .get_first_module::<GatewayClientModule>()
            .gateway_subscribe_ln_receive(intercept_op)
            .await?
            .into_stream();
        assert_eq!(intercept_sub.ok().await?, GatewayExtReceiveStates::Funding);
        assert_matches!(
            intercept_sub.ok().await?,
            GatewayExtReceiveStates::Preimage { .. }
        );
        assert_eq!(
            initial_gateway_balance - invoice_amount,
            gateway_client.get_balance().await
        );

        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_client_intercept_offer_does_not_exist() -> anyhow::Result<()> {
    single_federation_test(|gateway, _, fed, _, _| async move {
        let gateway_client = gateway.select_client(fed.id()).await;
        // Print money for gateway client
        let initial_gateway_balance = sats(1000);
        let dummy_module = gateway_client.get_first_module::<DummyClientModule>();
        let (_, outpoint) = dummy_module.print_money(initial_gateway_balance).await?;
        dummy_module.receive_money(outpoint).await?;
        assert_eq!(gateway_client.get_balance().await, sats(1000));

        // Create HTLC that doesn't correspond to an offer in the federation
        let htlc = Htlc {
            payment_hash: sha256(&[15]),
            incoming_amount_msat: Amount::from_msats(100),
            outgoing_amount_msat: Amount::from_msats(100),
            incoming_expiry: u32::MAX,
            short_channel_id: Some(1),
            incoming_chan_id: 2,
            htlc_id: 1,
        };

        match gateway_client
            .get_first_module::<GatewayClientModule>()
            .gateway_handle_intercepted_htlc(htlc)
            .await
        {
            Ok(_) => panic!(
                "Expected incoming offer validation to fail because the offer does not exist"
            ),
            Err(e) => assert_eq!(e.to_string(), "Timed out fetching the offer".to_string()),
        }

        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_client_intercept_htlc_no_funds() -> anyhow::Result<()> {
    single_federation_test(|gateway, _, fed, user_client, _| async move {
        let gateway_id = gateway.gateway.gateway_id;
        let gateway_client = gateway.select_client(fed.id()).await;
        // User client creates invoice in federation
        let ln_module = user_client.get_first_module::<LightningClientModule>();
        let ln_gateway = ln_module.select_gateway(&gateway_id).await;
        let desc = Description::new("description".to_string())?;
        let (_invoice_op, invoice, _) = ln_module
            .create_bolt11_invoice(
                sats(100),
                Bolt11InvoiceDescription::Direct(&desc),
                None,
                "test intercept htlc but with no funds",
                ln_gateway,
            )
            .await?;

        // Run gateway state machine
        let htlc = Htlc {
            payment_hash: *invoice.payment_hash(),
            incoming_amount_msat: Amount::from_msats(invoice.amount_milli_satoshis().unwrap()),
            outgoing_amount_msat: Amount::from_msats(invoice.amount_milli_satoshis().unwrap()),
            incoming_expiry: u32::MAX,
            short_channel_id: Some(1),
            incoming_chan_id: 2,
            htlc_id: 1,
        };

        // Attempt to route an HTLC while the gateway has no funds
        match gateway_client
            .get_first_module::<GatewayClientModule>()
            .gateway_handle_intercepted_htlc(htlc)
            .await
        {
            Ok(_) => panic!("Expected incoming offer validation to fail due to lack of funds"),
            Err(e) => assert_eq!(e.to_string(), "Insufficient funds".to_string()),
        }

        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_client_intercept_htlc_invalid_offer() -> anyhow::Result<()> {
    single_federation_test(
        |gateway, other_lightning_client, fed, user_client, _| async move {
            let gateway_client = gateway.select_client(fed.id()).await;
            // Print money for gateway client
            let initial_gateway_balance = sats(1000);
            let gateway_dummy_module = gateway_client.get_first_module::<DummyClientModule>();
            let (_, outpoint) = gateway_dummy_module
                .print_money(initial_gateway_balance)
                .await?;
            gateway_dummy_module.receive_money(outpoint).await?;
            assert_eq!(gateway_client.get_balance().await, sats(1000));

            // Create test invoice
            let invoice = other_lightning_client.unpayable_invoice(sats(250), None);

            // Create offer with a preimage that doesn't correspond to the payment hash of
            // the invoice
            let user_lightning_module = user_client.get_first_module::<LightningClientModule>();

            let amount = sats(100);
            let preimage = BYTE_33;
            let ln_output = LightningOutput::new_v0_offer(IncomingContractOffer {
                amount,
                hash: *invoice.payment_hash(),
                encrypted_preimage: EncryptedPreimage::new(
                    &PreimageKey(preimage),
                    &user_lightning_module.cfg.threshold_pub_key,
                ),
                expiry_time: None,
            });
            // The client's receive state machine can be empty because the gateway should
            // not fund this contract
            let state_machines = Arc::new(move |_txid: TransactionId, _input_idx: u64| {
                Vec::<LightningClientStateMachines>::new()
            });
            let client_output = ClientOutput {
                output: ln_output,
                amount: Amount::ZERO,
                state_machines,
            };
            let tx = TransactionBuilder::new()
                .with_output(client_output.into_dyn(user_lightning_module.id));
            let operation_meta_gen = |txid, _| LightningOperationMeta {
                variant: LightningOperationMetaVariant::Receive {
                    out_point: OutPoint { txid, out_idx: 0 },
                    invoice: invoice.clone(),
                    gateway_id: None,
                },
                extra_meta: serde_json::to_value("test intercept HTLC with invalid offer")
                    .expect("Failed to serialize string into json"),
            };

            let operation_id = OperationId(invoice.payment_hash().to_byte_array());
            let (txid, _) = user_client
                .finalize_and_submit_transaction(
                    operation_id,
                    fedimint_ln_common::KIND.as_str(),
                    operation_meta_gen,
                    tx,
                )
                .await?;
            user_client
                .transaction_updates(operation_id)
                .await
                .await_tx_accepted(txid)
                .await
                .unwrap();

            // Run gateway state machine
            let htlc = Htlc {
                payment_hash: *invoice.payment_hash(),
                incoming_amount_msat: Amount::from_msats(invoice.amount_milli_satoshis().unwrap()),
                outgoing_amount_msat: Amount::from_msats(invoice.amount_milli_satoshis().unwrap()),
                incoming_expiry: u32::MAX,
                short_channel_id: Some(1),
                incoming_chan_id: 2,
                htlc_id: 1,
            };

            let intercept_op = gateway_client
                .get_first_module::<GatewayClientModule>()
                .gateway_handle_intercepted_htlc(htlc)
                .await?;
            let mut intercept_sub = gateway_client
                .get_first_module::<GatewayClientModule>()
                .gateway_subscribe_ln_receive(intercept_op)
                .await?
                .into_stream();
            assert_matches!(intercept_sub.ok().await?, GatewayExtReceiveStates::Funding);

            match intercept_sub.ok().await? {
                GatewayExtReceiveStates::RefundSuccess {
                    out_points,
                    error: _,
                } => {
                    // Assert that the gateway got it's refund
                    for outpoint in out_points {
                        gateway_dummy_module.receive_money(outpoint).await?;
                    }

                    assert_eq!(initial_gateway_balance, gateway_client.get_balance().await);
                }
                unexpected_state => panic!(
                    "Gateway receive state machine entered unexpected state: {unexpected_state:?}"
                ),
            }

            Ok(())
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_cannot_pay_expired_invoice() -> anyhow::Result<()> {
    single_federation_test(
        |gateway, other_lightning_client, fed, user_client, _| async move {
            let gateway_id = gateway.gateway.gateway_id;
            let gateway_client = gateway.select_client(fed.id()).await;
            let invoice = other_lightning_client
                .invoice(sats(1000), 1.into())
                .unwrap();
            assert_eq!(invoice.expiry_time(), Duration::from_secs(1));

            // at seconds granularity, must wait `expiry + 1s` to make sure expired
            sleep_in_test("waiting for invoice to expire", Duration::from_secs(2)).await;

            // Print money for user_client
            let dummy_module = user_client.get_first_module::<DummyClientModule>();
            let (_, outpoint) = dummy_module.print_money(sats(2000)).await?;
            dummy_module.receive_money(outpoint).await?;
            assert_eq!(user_client.get_balance().await, sats(2000));

            // User client pays test invoice
            let lightning_module = user_client.get_first_module::<LightningClientModule>();
            let gateway_module = lightning_module.select_gateway(&gateway_id).await;
            let OutgoingLightningPayment {
                payment_type,
                contract_id,
                fee: _,
            } = user_pay_invoice(&lightning_module, invoice.clone(), &gateway_id).await?;
            match payment_type {
                PayType::Lightning(pay_op) => {
                    let mut pay_sub = lightning_module
                        .subscribe_ln_pay(pay_op)
                        .await?
                        .into_stream();
                    assert_eq!(pay_sub.ok().await?, LnPayState::Created);
                    let funded = pay_sub.ok().await?;
                    assert_matches!(funded, LnPayState::Funded { .. });

                    let payload = PayInvoicePayload {
                        federation_id: user_client.federation_id(),
                        contract_id,
                        payment_data: get_payment_data(gateway_module, invoice),
                        preimage_auth: Hash::hash(&[0; 32]),
                    };

                    let gw_pay_op = gateway_client
                        .get_first_module::<GatewayClientModule>()
                        .gateway_pay_bolt11_invoice(payload)
                        .await?;
                    let mut gw_pay_sub = gateway_client
                        .get_first_module::<GatewayClientModule>()
                        .gateway_subscribe_ln_pay(gw_pay_op)
                        .await?
                        .into_stream();

                    assert_eq!(gw_pay_sub.ok().await?, GatewayExtPayStates::Created);
                    assert_matches!(gw_pay_sub.ok().await?, GatewayExtPayStates::Canceled { .. });
                }
                _ => panic!("Expected Lightning payment!"),
            }

            // Balance should be unchanged
            assert_eq!(gateway_client.get_balance().await, sats(0));

            Ok(())
        },
    )
    .await
}

// TODO: fix and re-enable https://github.com/fedimint/fedimint/issues/5001
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_configuration() -> anyhow::Result<()> {
    let fixtures = fixtures();

    let fed = fixtures.new_default_fed().await;
    let gateway = fixtures.new_gateway(0, None).await;
    let initial_rpc_client = gateway.get_rpc();

    // Verify that we can't join a federation yet because the configuration is not
    // set
    let join_payload = ConnectFedPayload {
        invite_code: fed.invite_code().to_string(),
    };

    verify_gateway_rpc_failure(
        "connect_federation",
        || initial_rpc_client.connect_federation(join_payload.clone()),
        StatusCode::NOT_FOUND,
    )
    .await;

    // Verify that the gateway's state is "Configuring"
    let gw_info = verify_gateway_rpc_success("get_info", || initial_rpc_client.get_info()).await;
    assert_eq!(gw_info.gateway_state, "Configuring".to_string());

    // Verify that the gateway's fees, and network are `None`
    assert_eq!(gw_info.fees, None);
    assert_eq!(gw_info.network, None);

    let test_password = "test_password".to_string();
    let set_configuration_payload = SetConfigurationPayload {
        password: Some(test_password.clone()),
        num_route_hints: None,
        routing_fees: None,
        network: None,
        per_federation_routing_fees: None,
    };
    verify_gateway_rpc_success("set_configuration", || {
        initial_rpc_client.set_configuration(set_configuration_payload.clone())
    })
    .await;

    // Verify that the gateway's password is stored correctly (i.e. the stored hash
    // and salt match the password)
    let gateway_config = gateway
        .gateway
        .gateway_config
        .read()
        .await
        .clone()
        .expect("Gateway config should be set");
    assert_eq!(
        gateway_config.hashed_password,
        hash_password(&test_password, gateway_config.password_salt)
    );

    // Verify client with no password fails since the password has been set
    verify_gateway_rpc_failure(
        "get_info",
        || initial_rpc_client.get_info(),
        StatusCode::UNAUTHORIZED,
    )
    .await;

    // Verify the gateway's state is "Running" with default fee and default or
    // lightning node network
    let initial_rpc_client_with_password = initial_rpc_client.with_password(Some(test_password));
    let gw_info =
        verify_gateway_rpc_success("get_info", || initial_rpc_client_with_password.get_info())
            .await;
    assert_eq!(gw_info.gateway_state, "Running".to_string());
    assert_eq!(gw_info.fees, Some(DEFAULT_FEES));
    assert_eq!(gw_info.network, Some(DEFAULT_NETWORK));

    // Verify we can change configurations when the gateway is running
    let new_password = "new_password".to_string();
    let fee = "10,10000".to_string();
    let federation_fee = FederationRoutingFees::from_str(&fee)?;
    let set_configuration_payload = SetConfigurationPayload {
        password: Some(new_password.clone()),
        num_route_hints: Some(1),
        routing_fees: Some(federation_fee.clone()),
        network: None,
        per_federation_routing_fees: None,
    };
    verify_gateway_rpc_success("set_configuration", || {
        initial_rpc_client_with_password.set_configuration(set_configuration_payload.clone())
    })
    .await;

    // Verify info works with the new password.
    let new_password_rpc_client = initial_rpc_client.with_password(Some(new_password.clone()));
    let gw_info =
        verify_gateway_rpc_success("get_info", || new_password_rpc_client.get_info()).await;

    assert_eq!(gw_info.gateway_state, "Running".to_string());
    assert_eq!(gw_info.fees, Some(GatewayFee(federation_fee.into()).0));
    assert_eq!(gw_info.network, Some(DEFAULT_NETWORK));

    // Verify that get_info with the old password fails
    verify_gateway_rpc_failure(
        "get_info",
        || initial_rpc_client_with_password.get_info(),
        StatusCode::UNAUTHORIZED,
    )
    .await;

    // Verify we can configure gateway to a network same as than the lightning nodes
    let set_configuration_payload = SetConfigurationPayload {
        password: Some(new_password.clone()),
        num_route_hints: None,
        network: Some(DEFAULT_NETWORK), /* Same as connected
                                         * lightning node's
                                         * network */
        routing_fees: None,
        per_federation_routing_fees: None,
    };
    verify_gateway_rpc_success("set_configuration", || {
        new_password_rpc_client.set_configuration(set_configuration_payload.clone())
    })
    .await;

    // Verify we cannot reconfigure gateway to a network different than the
    // lightning nodes
    let set_configuration_payload = SetConfigurationPayload {
        password: Some(new_password.clone()),
        num_route_hints: None,
        network: Some(Network::Testnet), /* Different from
                                          * connected lightning
                                          * node's network */
        routing_fees: None,
        per_federation_routing_fees: None,
    };
    verify_gateway_rpc_failure(
        "set_configuration",
        || new_password_rpc_client.set_configuration(set_configuration_payload.clone()),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .await;

    // Verify we can connect to a federation if the gateway is configured to use
    // the same network. Test federations are on Regtest by default
    verify_gateway_rpc_success("connect_federation", || {
        new_password_rpc_client.connect_federation(join_payload.clone())
    })
    .await;

    verify_gateway_rpc_success("get_balance", || {
        new_password_rpc_client.get_balance(BalancePayload {
            federation_id: fed.invite_code().federation_id(),
        })
    })
    .await;

    // Verify we can configure gateway to charge fees for specific federation
    let federation_routing_fees = FederationRoutingFees::from_str("10,10000")?;
    let set_configuration_payload = SetConfigurationPayload {
        password: None,
        num_route_hints: None,
        routing_fees: None,
        network: None,
        per_federation_routing_fees: Some(vec![(fed.id(), federation_routing_fees.clone())]),
    };
    verify_gateway_rpc_success("set_configuration", || {
        new_password_rpc_client.set_configuration(set_configuration_payload.clone())
    })
    .await;
    // Verify info has new per federation routing fees.
    let new_password_rpc_client = initial_rpc_client.with_password(Some(new_password.clone()));
    let gw_info =
        verify_gateway_rpc_success("get_info", || new_password_rpc_client.get_info()).await;
    assert_eq!(
        gw_info
            .federations
            .iter()
            .find(|f| f.federation_id == fed.id())
            .and_then(|f| f.routing_fees.clone()),
        Some(federation_routing_fees)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_supports_connecting_multiple_federations() -> anyhow::Result<()> {
    multi_federation_test(|gateway, rpc, fed1, fed2, _| async move {
        info!("Starting test_gateway_supports_connecting_multiple_federations");
        assert_eq!(rpc.get_info().await.unwrap().federations.len(), 0);

        let invite1 = fed1.invite_code();
        let info = rpc
            .connect_federation(ConnectFedPayload {
                invite_code: invite1.to_string(),
            })
            .await
            .unwrap();

        assert_eq!(info.federation_id, invite1.federation_id());

        let invite2 = fed2.invite_code();
        let info = rpc
            .connect_federation(ConnectFedPayload {
                invite_code: invite2.to_string(),
            })
            .await
            .unwrap();
        assert_eq!(info.federation_id, invite2.federation_id());
        drop(gateway); // keep until the end to avoid the gateway shutting down too early
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_shows_info_about_all_connected_federations() -> anyhow::Result<()> {
    multi_federation_test(|gateway, rpc, fed1, fed2, _| async move {
        assert_eq!(rpc.get_info().await.unwrap().federations.len(), 0);

        let id1 = fed1.invite_code().federation_id();
        let id2 = fed2.invite_code().federation_id();

        connect_federations(&rpc, &[fed1, fed2]).await.unwrap();

        let info = rpc.get_info().await.unwrap();

        assert_eq!(info.federations.len(), 2);
        assert!(info
            .federations
            .iter()
            .any(|info| info.federation_id == id1 && info.balance_msat == Amount::ZERO));
        assert!(info
            .federations
            .iter()
            .any(|info| info.federation_id == id2 && info.balance_msat == Amount::ZERO));
        drop(gateway); // keep until the end to avoid the gateway shutting down too early
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_can_leave_connected_federations() -> anyhow::Result<()> {
    multi_federation_test(|gateway, rpc, fed1, fed2, _| async move {
        assert_eq!(rpc.get_info().await.unwrap().federations.len(), 0);

        let invite1 = fed1.invite_code();
        let invite2 = fed2.invite_code();

        let id1 = invite1.federation_id();
        let id2 = invite2.federation_id();

        connect_federations(&rpc, &[fed1, fed2]).await.unwrap();

        let info = rpc.get_info().await.unwrap();
        assert_eq!(info.federations.len(), 2);
        assert!(info
            .federations
            .iter()
            .any(|info| info.federation_id == id1 && info.channel_id == Some(1)));
        assert!(info
            .federations
            .iter()
            .any(|info| info.federation_id == id2 && info.channel_id == Some(2)));

        // remove first connected federation
        let fed_info = rpc
            .leave_federation(LeaveFedPayload { federation_id: id1 })
            .await
            .unwrap();
        assert_eq!(fed_info.federation_id, id1);
        assert_eq!(fed_info.channel_id, Some(1));

        // reconnect the first federation
        let fed_info = rpc
            .connect_federation(ConnectFedPayload {
                invite_code: invite1.to_string(),
            })
            .await
            .unwrap();
        assert_eq!(fed_info.federation_id, id1);
        assert_eq!(fed_info.channel_id, Some(3));

        // remove second connected federation
        let fed_info = rpc
            .leave_federation(LeaveFedPayload { federation_id: id2 })
            .await
            .unwrap();
        assert_eq!(fed_info.federation_id, id2);
        assert_eq!(fed_info.channel_id, Some(2));

        // reconnect the second federation
        let fed_info = rpc
            .connect_federation(ConnectFedPayload {
                invite_code: invite2.to_string(),
            })
            .await
            .unwrap();
        assert_eq!(fed_info.federation_id, id2);
        assert_eq!(fed_info.channel_id, Some(4));

        let info = rpc.get_info().await.unwrap();
        assert_eq!(info.federations.len(), 2);
        assert_eq!(
            info.channels.unwrap().keys().cloned().collect::<Vec<u64>>(),
            vec![3, 4]
        );

        drop(gateway); // keep until the end to avoid the gateway shutting down too early
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_shows_balance_for_any_connected_federation() -> anyhow::Result<()> {
    multi_federation_test(|gateway, rpc, fed1, fed2, _| async move {
        let id1 = fed1.invite_code().federation_id();
        let id2 = fed2.invite_code().federation_id();

        connect_federations(&rpc, &[fed1, fed2]).await.unwrap();

        let pre_balances = get_balances(&rpc, &[id1, id2]).await;

        send_msats_to_gateway(&gateway, id1, 5_000).await;
        send_msats_to_gateway(&gateway, id2, 1_000).await;

        let post_balances = get_balances(&rpc, &[id1, id2]).await;

        assert_eq!(pre_balances[0], 0);
        assert_eq!(pre_balances[1], 0);
        assert_eq!(post_balances[0], 5_000);
        assert_eq!(post_balances[1], 1_000);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gateway_executes_swaps_between_connected_federations() -> anyhow::Result<()> {
    multi_federation_test(|gateway, rpc, fed1, fed2, _| async move {
        let gateway_id = gateway.gateway.gateway_id;
        let id1 = fed1.invite_code().federation_id();
        let id2 = fed2.invite_code().federation_id();

        connect_federations(&rpc, &[fed1.clone(), fed2.clone()])
            .await
            .unwrap();

        // setting specific routing fees for fed1
        let fed_routing_fees = FederationRoutingFees::from_str("10,10000")?;
        let set_configuration_payload = SetConfigurationPayload {
            password: None,
            num_route_hints: None,
            routing_fees: None,
            network: None,
            per_federation_routing_fees: Some(vec![(id1, fed_routing_fees.clone())]),
        };
        verify_gateway_rpc_success("set_configuration", || {
            rpc.set_configuration(set_configuration_payload.clone())
        })
        .await;

        send_msats_to_gateway(&gateway, id1, 10_000).await;
        send_msats_to_gateway(&gateway, id2, 10_000).await;

        let client1 = fed1.new_client().await;
        let client2 = fed2.new_client().await;

        // Check gateway balances before facilitating direct swap between federations
        let pre_balances = get_balances(&rpc, &[id1, id2]).await;
        assert_eq!(pre_balances[0], 10_000);
        assert_eq!(pre_balances[1], 10_000);

        let deposit_amt = msats(5_000);
        let client1_dummy_module = client1.get_first_module::<DummyClientModule>();
        let (_, outpoint) = client1_dummy_module.print_money(deposit_amt).await?;
        client1_dummy_module.receive_money(outpoint).await?;
        assert_eq!(client1.get_balance().await, deposit_amt);

        // User creates invoice in federation 2
        let invoice_amt = msats(2_500);
        let ln_module = client2.get_first_module::<LightningClientModule>();
        let ln_gateway = ln_module.select_gateway(&gateway_id).await;
        let desc = Description::new("description".to_string())?;
        let (receive_op, invoice, _) = ln_module
            .create_bolt11_invoice(
                invoice_amt,
                Bolt11InvoiceDescription::Direct(&desc),
                None,
                "test gw swap between federations",
                ln_gateway,
            )
            .await?;
        let mut receive_sub = ln_module
            .subscribe_ln_receive(receive_op)
            .await?
            .into_stream();

        // A client pays invoice in federation 1
        let gateway_client = gateway.select_client(id1).await;
        gateway_pay_valid_invoice(
            invoice,
            &client1,
            &gateway_client,
            &gateway.gateway.gateway_id,
        )
        .await?;

        // A client receives cash via swap in federation 2
        assert_eq!(receive_sub.ok().await?, LnReceiveState::Created);
        let waiting_payment = receive_sub.ok().await?;
        assert_matches!(waiting_payment, LnReceiveState::WaitingForPayment { .. });
        let funded = receive_sub.ok().await?;
        assert_matches!(funded, LnReceiveState::Funded);
        let waiting_funds = receive_sub.ok().await?;
        assert_matches!(waiting_funds, LnReceiveState::AwaitingFunds { .. });
        let claimed = receive_sub.ok().await?;
        assert_matches!(claimed, LnReceiveState::Claimed);
        assert_eq!(client2.get_balance().await, invoice_amt);

        // Check gateway balances after facilitating direct swap between federations
        let gateway_fed1_balance = gateway_client.get_balance().await;
        let gateway_fed2_client = gateway.select_client(id2).await;
        let gateway_fed2_balance = gateway_fed2_client.get_balance().await;

        // Balance in gateway of sending federation is deducted the invoice amount
        assert_eq!(
            gateway_fed2_balance.msats,
            pre_balances[1] - invoice_amt.msats
        );

        let fee = routing_fees_in_msats(&fed_routing_fees, &invoice_amt);

        // Balance in gateway of receiving federation is increased `invoice_amt` + `fee`
        assert_eq!(
            gateway_fed1_balance.msats,
            pre_balances[0] + invoice_amt.msats + fee
        );

        Ok(())
    })
    .await
}

fn routing_fees_in_msats(routing_fees: &FederationRoutingFees, amount: &Amount) -> u64 {
    ((amount.msats * routing_fees.proportional_millionths as u64) / 1_000_000)
        + routing_fees.base_msat as u64
}

async fn reconnect_federation(rpc: &GatewayRpcClient, fed: &FederationTest) {
    verify_gateway_rpc_success("leave_federation", || {
        rpc.leave_federation(LeaveFedPayload {
            federation_id: fed.id(),
        })
    })
    .await;
    verify_gateway_rpc_success("connect_federation", || {
        rpc.connect_federation(ConnectFedPayload {
            invite_code: fed.invite_code().to_string(),
        })
    })
    .await;
}

/// Verifies that a gateway RPC succeeds. If it fails, the status code of the
/// RPC is printed.
async fn verify_gateway_rpc_success<Fut, T>(name: &str, func: impl Fn() -> Fut) -> T
where
    Fut: Future<Output = GatewayRpcResult<T>>,
{
    match func().await {
        Ok(ret) => ret,
        Err(GatewayRpcError::RequestError(e)) => panic!("RequestError during {name}: {e:?}"),
        Err(GatewayRpcError::BadStatus(status)) => {
            panic!("{name} returned error code {status} when success was expected")
        }
    }
}

/// Verifies that a gateway RPC fails with a specific `StatusCode`
async fn verify_gateway_rpc_failure<Fut, T>(
    name: &str,
    func: impl Fn() -> Fut,
    status_code: StatusCode,
) where
    Fut: Future<Output = GatewayRpcResult<T>>,
{
    match func().await {
        Ok(_) => panic!("{name} returned success, expected {status_code}"),
        Err(GatewayRpcError::RequestError(e)) => panic!("RequestError during {name}: {e:?}"),
        Err(GatewayRpcError::BadStatus(status)) => {
            assert_eq!(
                status, status_code,
                "Unexpected status code returned. Expected: {status_code}, found {status}"
            )
        }
    }
}

/// Connects the gateway to all federations in `feds`.
async fn connect_federations(
    rpc: &GatewayRpcClient,
    feds: &[FederationTest],
) -> anyhow::Result<()> {
    for fed in feds {
        let invite_code = fed.invite_code().to_string();
        rpc.connect_federation(ConnectFedPayload { invite_code })
            .await?;
    }
    Ok(())
}

/// Retrieves the balance of each federation the gateway is connected to.
async fn get_balances(
    rpc: &GatewayRpcClient,
    ids: impl IntoIterator<Item = &FederationId>,
) -> Vec<u64> {
    let mut balances = vec![];
    for id in ids.into_iter() {
        balances.push(
            rpc.get_balance(BalancePayload { federation_id: *id })
                .await
                .unwrap()
                .msats,
        )
    }

    balances
}

/// Prints msats for the gateway using the dummy module.
async fn send_msats_to_gateway(gateway: &GatewayTest, id: FederationId, msats: u64) {
    let client = gateway.select_client(id).await;
    let dummy_module = client.get_first_module::<DummyClientModule>();
    let (_, outpoint) = dummy_module
        .print_money(Amount::from_msats(msats))
        .await
        .unwrap();
    dummy_module.receive_money(outpoint).await.unwrap();
}
