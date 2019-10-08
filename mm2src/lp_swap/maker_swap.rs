#![cfg_attr(not(feature = "native"), allow(dead_code))]

use bitcrypto::dhash160;
use coins::FoundSwapTxSpend;
use crc::crc32;
use peers::FixedValidator;
use rand::Rng;
use super::*;

pub fn stats_maker_swap_file_path(ctx: &MmArc, uuid: &str) -> PathBuf {
    ctx.dbdir().join("SWAPS").join("STATS").join("MAKER").join(format!("{}.json", uuid))
}

fn save_my_maker_swap_event(ctx: &MmArc, swap: &MakerSwap, event: MakerSavedEvent) -> Result<(), String> {
    let path = my_swap_file_path(ctx, &swap.uuid);
    let content = slurp(&path);
    let swap: SavedSwap = if content.is_empty() {
        SavedSwap::Maker(MakerSavedSwap {
            uuid: swap.uuid.clone(),
            maker_amount: Some(swap.maker_amount.clone()),
            maker_coin: Some(swap.maker_coin.ticker().to_owned()),
            taker_amount: Some(swap.taker_amount.clone()),
            taker_coin: Some(swap.taker_coin.ticker().to_owned()),
            gui: ctx.gui().map(|g| g.to_owned()),
            mm_version: Some(MM_VERSION.to_owned()),
            events: vec![],
            success_events: vec!["Started".into(), "Negotiated".into(), "TakerFeeValidated".into(),
                                 "MakerPaymentSent".into(), "TakerPaymentReceived".into(),
                                 "TakerPaymentWaitConfirmStarted".into(), "TakerPaymentValidatedAndConfirmed".into(),
                                 "TakerPaymentSpent".into(), "Finished".into()],
            error_events: vec!["StartFailed".into(), "NegotiateFailed".into(), "TakerFeeValidateFailed".into(),
                               "MakerPaymentTransactionFailed".into(), "MakerPaymentDataSendFailed".into(),
                               "TakerPaymentValidateFailed".into(), "TakerPaymentSpendFailed".into(), "MakerPaymentRefunded".into(),
                               "MakerPaymentRefundFailed".into()],
        })
    } else {
        try_s!(json::from_slice(&content))
    };

    if let SavedSwap::Maker(mut maker_swap) = swap {
        maker_swap.events.push(event);
        let new_swap = SavedSwap::Maker(maker_swap);
        let new_content = try_s!(json::to_vec(&new_swap));
        let mut file = try_s!(File::create(path));
        try_s!(file.write_all(&new_content));
        Ok(())
    } else {
        ERR!("Expected SavedSwap::Maker at {}, got {:?}", path.display(), swap)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TakerNegotiationData {
    pub taker_payment_locktime: u64,
    pub taker_pubkey: H264Json,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct MakerSwapData {
    taker_coin: String,
    maker_coin: String,
    taker: H256Json,
    secret: H256Json,
    secret_hash: Option<H160Json>,
    my_persistent_pub: H264Json,
    lock_duration: u64,
    maker_amount: BigDecimal,
    taker_amount: BigDecimal,
    maker_payment_confirmations: u64,
    taker_payment_confirmations: u64,
    maker_payment_lock: u64,
    /// Allows to recognize one SWAP from the other in the logs. #274.
    uuid: String,
    started_at: u64,
    maker_coin_start_block: u64,
    taker_coin_start_block: u64,
}

pub struct MakerSwap {
    ctx: MmArc,
    maker_coin: MmCoinEnum,
    taker_coin: MmCoinEnum,
    maker_amount: BigDecimal,
    taker_amount: BigDecimal,
    my_persistent_pub: H264,
    taker: bits256,
    uuid: String,
    data: MakerSwapData,
    taker_payment_lock: u64,
    other_persistent_pub: H264,
    taker_fee: Option<TransactionDetails>,
    maker_payment: Option<TransactionDetails>,
    taker_payment: Option<TransactionDetails>,
    taker_payment_confirmed: bool,
    taker_payment_spend: Option<TransactionDetails>,
    maker_payment_refund: Option<TransactionDetails>,
    errors: Vec<SwapError>,
    finished_at: u64,
}

impl MakerSwap {
    fn apply_event(&mut self, event: MakerSwapEvent) -> Result<(), String> {
        match event {
            MakerSwapEvent::Started(data) => self.data = data,
            MakerSwapEvent::StartFailed(err) => self.errors.push(err),
            MakerSwapEvent::Negotiated(data) => {
                self.taker_payment_lock = data.taker_payment_locktime;
                self.other_persistent_pub = data.taker_pubkey.into();
            },
            MakerSwapEvent::NegotiateFailed(err) => self.errors.push(err),
            MakerSwapEvent::TakerFeeValidated(tx) => self.taker_fee = Some(tx),
            MakerSwapEvent::TakerFeeValidateFailed(err) => self.errors.push(err),
            MakerSwapEvent::MakerPaymentSent(tx) => self.maker_payment = Some(tx),
            MakerSwapEvent::MakerPaymentTransactionFailed(err) => self.errors.push(err),
            MakerSwapEvent::MakerPaymentDataSendFailed(err) => self.errors.push(err),
            MakerSwapEvent::TakerPaymentReceived(tx) => self.taker_payment = Some(tx),
            MakerSwapEvent::TakerPaymentWaitConfirmStarted => (),
            MakerSwapEvent::TakerPaymentValidatedAndConfirmed => self.taker_payment_confirmed = true,
            MakerSwapEvent::TakerPaymentValidateFailed(err) => self.errors.push(err),
            MakerSwapEvent::TakerPaymentSpent(tx) => self.taker_payment_spend = Some(tx),
            MakerSwapEvent::TakerPaymentSpendFailed(err) => self.errors.push(err),
            MakerSwapEvent::MakerPaymentRefunded(tx) => self.maker_payment_refund = Some(tx),
            MakerSwapEvent::MakerPaymentRefundFailed(err) => self.errors.push(err),
            MakerSwapEvent::Finished => self.finished_at = now_ms() / 1000,
        }
        Ok(())
    }

    fn handle_command(&self, command: MakerSwapCommand)
                      -> Result<(Option<MakerSwapCommand>, Vec<MakerSwapEvent>), String> {
        match command {
            MakerSwapCommand::Start => self.start(),
            MakerSwapCommand::Negotiate => self.negotiate(),
            MakerSwapCommand::WaitForTakerFee => self.wait_taker_fee(),
            MakerSwapCommand::SendPayment => self.maker_payment(),
            MakerSwapCommand::WaitForTakerPayment => self.wait_for_taker_payment(),
            MakerSwapCommand::ValidateTakerPayment => self.validate_taker_payment(),
            MakerSwapCommand::SpendTakerPayment => self.spend_taker_payment(),
            MakerSwapCommand::RefundMakerPayment => self.refund_maker_payment(),
            MakerSwapCommand::Finish => Ok((None, vec![MakerSwapEvent::Finished])),
        }
    }

    pub fn new(
        ctx: MmArc,
        taker: bits256,
        maker_coin: MmCoinEnum,
        taker_coin: MmCoinEnum,
        maker_amount: BigDecimal,
        taker_amount: BigDecimal,
        my_persistent_pub: H264,
        uuid: String,
    ) -> Self {
        MakerSwap {
            ctx: ctx.clone(),
            maker_coin,
            taker_coin,
            maker_amount,
            taker_amount,
            my_persistent_pub,
            taker,
            uuid,
            data: MakerSwapData::default(),
            taker_payment_lock: 0,
            other_persistent_pub: H264::default(),
            taker_fee: None,
            maker_payment: None,
            taker_payment: None,
            taker_payment_spend: None,
            maker_payment_refund: None,
            errors: vec![],
            finished_at: 0,
            taker_payment_confirmed: false,
        }
    }

    fn start(&self) -> Result<(Option<MakerSwapCommand>, Vec<MakerSwapEvent>), String> {
        let my_balance = match self.maker_coin.my_balance().wait() {
            Ok(balance) => balance,
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::StartFailed(ERRL!("!my_balance {}", e).into())],
            ))
        };

        let locked = get_locked_amount_by_other_swaps(&self.ctx, &self.uuid, self.maker_coin.ticker());
        let available = &my_balance - &locked;
        if self.maker_amount > available {
            return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::StartFailed(ERRL!("maker amount {} is larger than available {}, balance {}, locked by other swaps {}",
                    self.maker_amount, available, my_balance, locked
                ).into())],
            ));
        }

        if let Err(e) = self.maker_coin.check_i_have_enough_to_trade(&self.maker_amount.clone().into(), &my_balance.clone().into(), TradeInfo::Maker).wait() {
            return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::StartFailed(ERRL!("!check_i_have_enough_to_trade {}", e).into())],
            ));
        };

        if let Err(e) = self.taker_coin.can_i_spend_other_payment().wait() {
            return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::StartFailed(ERRL!("!can_i_spend_other_payment {}", e).into())],
            ));
        };

        let lock_duration = lp_atomic_locktime(self.maker_coin.ticker(), self.taker_coin.ticker());
        let mut rng = rand::thread_rng();
        let secret: [u8; 32] = rng.gen();
        let started_at = now_ms() / 1000;

        let maker_coin_start_block = match self.maker_coin.current_block().wait() {
            Ok(b) => b,
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::StartFailed(ERRL!("!maker_coin.current_block {}", e).into())],
            ))
        };

        let taker_coin_start_block = match self.taker_coin.current_block().wait() {
            Ok(b) => b,
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::StartFailed(ERRL!("!taker_coin.current_block {}", e).into())],
            ))
        };

        let data = MakerSwapData {
            taker_coin: self.taker_coin.ticker().to_owned(),
            maker_coin: self.maker_coin.ticker().to_owned(),
            taker: self.taker.bytes.into(),
            secret_hash: Some(dhash160(&secret).into()),
            secret: secret.into(),
            started_at,
            lock_duration,
            maker_amount: self.maker_amount.clone(),
            taker_amount: self.taker_amount.clone(),
            maker_payment_confirmations: self.maker_coin.required_confirmations(),
            taker_payment_confirmations: self.taker_coin.required_confirmations(),
            maker_payment_lock: started_at + lock_duration * 2,
            my_persistent_pub: self.my_persistent_pub.clone().into(),
            uuid: self.uuid.clone(),
            maker_coin_start_block,
            taker_coin_start_block,
        };

        Ok((Some(MakerSwapCommand::Negotiate), vec![MakerSwapEvent::Started(data)]))
    }

    fn negotiate(&self) -> Result<(Option<MakerSwapCommand>, Vec<MakerSwapEvent>), String> {
        let maker_negotiation_data = SwapNegotiationData {
            started_at: self.data.started_at,
            payment_locktime: self.data.maker_payment_lock,
            secret_hash: dhash160(&self.data.secret.0),
            persistent_pubkey: self.my_persistent_pub.clone(),
        };

        let bytes = serialize(&maker_negotiation_data);
        let sending_f = match send!(self.ctx, self.taker, fomat!(("negotiation") '@' (self.uuid)), 30, bytes.as_slice()) {
            Ok(f) => f,
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::NegotiateFailed(ERRL!("{}", e).into())],
            )),
        };

        let data = match recv!(self, sending_f, "negotiation-reply", 90, -2000, FixedValidator::AnythingGoes) {
            Ok(d) => d,
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::NegotiateFailed(ERRL!("{:?}", e).into())],
            )),
        };
        let taker_data: SwapNegotiationData = match deserialize(data.as_slice()) {
            Ok(d) => d,
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::NegotiateFailed(ERRL!("{:?}", e).into())],
            )),
        };
        let time_dif = (self.data.started_at as i64 - taker_data.started_at as i64).abs();
        if  time_dif > 60 {
            return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::NegotiateFailed(ERRL!("Started_at time_dif over 60 {}", time_dif).into())]
            ))
        }

        let expected_lock_time = taker_data.started_at + self.data.lock_duration;
        if taker_data.payment_locktime != expected_lock_time {
            return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::NegotiateFailed(ERRL!("taker_data.payment_locktime {} not equal to expected {}", taker_data.payment_locktime, expected_lock_time).into())]
            ))
        }

        Ok((
            Some(MakerSwapCommand::WaitForTakerFee),
            vec![MakerSwapEvent::Negotiated(
                TakerNegotiationData {
                    taker_payment_locktime: taker_data.payment_locktime,
                    taker_pubkey: taker_data.persistent_pubkey.into(),
                })
            ],
        ))
    }

    fn wait_taker_fee(&self) -> Result<(Option<MakerSwapCommand>, Vec<MakerSwapEvent>), String> {
        let negotiated = serialize(&true);
        let sending_f = match send!(self.ctx, self.taker, fomat!(("negotiated") '@' (self.uuid)), 30, negotiated.as_slice()) {
            Ok(f) => f,
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::NegotiateFailed(ERRL!("{}", e).into())],
            )),
        };

        let payload = match recv!(self, sending_f, "taker-fee", 600, -2003, FixedValidator::AnythingGoes) {
            Ok(d) => d,
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::TakerFeeValidateFailed(ERRL!("{}", e).into())]
            ))
        };
        let taker_fee = match self.taker_coin.tx_enum_from_bytes(&payload) {
            Ok(tx) => tx,
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::TakerFeeValidateFailed(ERRL!("{}", e).into())]
            ))
        };

        let hash = taker_fee.tx_hash();
        log!({ "Taker fee tx {:02x}", hash });

        let fee_addr_pub_key = unwrap!(hex::decode("03bc2c7ba671bae4a6fc835244c9762b41647b9827d4780a89a949b984a8ddcc06"));
        let fee_amount = dex_fee_amount(&self.data.maker_coin, &self.data.taker_coin, &self.taker_amount);

        let mut attempts = 0;
        loop {
            match self.taker_coin.validate_fee(&taker_fee, &fee_addr_pub_key, &fee_amount) {
                Ok(_) => break,
                Err(err) => if attempts >= 3 {
                    return Ok((
                        Some(MakerSwapCommand::Finish),
                        vec![MakerSwapEvent::TakerFeeValidateFailed(ERRL!("{}", err).into())]
                    ))
                } else {
                    attempts += 1;
                    thread::sleep(Duration::from_secs(10));
                }
            };
        };

        let mut attempts = 0;
        let fee_details = loop {
            match self.taker_coin.tx_details_by_hash(&hash) {
                Ok(details) => break details,
                Err(err) => if attempts >= 3 {
                    return Ok((
                        Some(MakerSwapCommand::Finish),
                        vec![MakerSwapEvent::TakerFeeValidateFailed(ERRL!("Taker fee tx_details_by_hash failed {}", err).into())]
                    ))
                } else {
                    attempts += 1;
                    thread::sleep(Duration::from_secs(10));
                }
            };
        };

        Ok((
            Some(MakerSwapCommand::SendPayment),
            vec![MakerSwapEvent::TakerFeeValidated(fee_details)]
        ))
    }

    fn maker_payment(&self) -> Result<(Option<MakerSwapCommand>, Vec<MakerSwapEvent>), String> {
        let timeout = self.data.started_at + self.data.lock_duration / 3;
        let now = now_ms() / 1000;
        if now > timeout {
            return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::MakerPaymentTransactionFailed(ERRL!("Timeout {} > {}", now, timeout).into())],
            ));
        }

        let transaction = match self.maker_coin.check_if_my_payment_sent(
            self.data.maker_payment_lock as u32,
            &*self.other_persistent_pub,
            &*dhash160(&self.data.secret.0),
            self.data.maker_coin_start_block,
        ) {
            Ok(res) => match res {
                Some(tx) => tx,
                None => {
                    let payment_fut = self.maker_coin.send_maker_payment(
                        self.data.maker_payment_lock as u32,
                        &*self.other_persistent_pub,
                        &*dhash160(&self.data.secret.0),
                        self.maker_amount.clone(),
                    );

                    match payment_fut.wait() {
                        Ok(t) => t,
                        Err(err) => return Ok((
                            Some(MakerSwapCommand::Finish),
                            vec![MakerSwapEvent::MakerPaymentTransactionFailed(ERRL!("{}", err).into())],
                        ))
                    }
                }
            },
            Err(e) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::MakerPaymentTransactionFailed(ERRL!("{}", e).into())],
            ))
        };

        let hash = transaction.tx_hash();
        log!({ "Maker payment tx {:02x}", hash });
        // we can attempt to get the details in loop here as transaction was already sent and
        // is present on blockchain so only transport errors are expected to happen
        let tx_details = loop {
            match self.maker_coin.tx_details_by_hash(&hash) {
                Ok(details) => break details,
                Err(e) => {
                    log!({"Error {} getting tx details of {:02x}", e, hash});
                    thread::sleep(Duration::from_secs(30));
                    continue;
                }
            }
        };

        Ok((
            Some(MakerSwapCommand::WaitForTakerPayment),
            vec![MakerSwapEvent::MakerPaymentSent(tx_details)]
        ))
    }

    fn wait_for_taker_payment(&self) -> Result<(Option<MakerSwapCommand>, Vec<MakerSwapEvent>), String> {
        let maker_payment_hex = self.maker_payment.as_ref().unwrap().tx_hex.clone();
        let sending_f = match send!(self.ctx, self.taker, fomat!(("maker-payment") '@' (self.uuid)), 60, maker_payment_hex) {
            Ok(f) => f,
            Err(e) => return Ok((
                Some(MakerSwapCommand::RefundMakerPayment),
                vec![MakerSwapEvent::MakerPaymentDataSendFailed(ERRL!("{}", e).into())]
            ))
        };

        let wait_duration = self.data.lock_duration / 3;
        let payload = match recv!(self, sending_f, "taker-payment", wait_duration, -2006, FixedValidator::AnythingGoes) {
            Ok(p) => p,
            Err(e) => return Ok((
                Some(MakerSwapCommand::RefundMakerPayment),
                vec![MakerSwapEvent::TakerPaymentValidateFailed(e.into())],
            ))
        };

        let taker_payment = match self.taker_coin.tx_enum_from_bytes(&payload) {
            Ok(tx) => tx,
            Err(err) => return Ok((
                Some(MakerSwapCommand::RefundMakerPayment),
                vec![MakerSwapEvent::TakerPaymentValidateFailed(ERRL!("!taker_coin.tx_enum_from_bytes: {}", err).into())]
            )),
        };

        let hash = taker_payment.tx_hash();
        log!({ "Taker payment tx {:02x}", hash });
        let mut attempts = 0;
        let tx_details = loop {
            match self.taker_coin.tx_details_by_hash(&hash) {
                Ok(details) => break details,
                Err(err) => if attempts >= 3 {
                    return Ok((
                        Some(MakerSwapCommand::RefundMakerPayment),
                        vec![MakerSwapEvent::TakerPaymentValidateFailed(ERRL!("!taker_coin.tx_details_by_hash: {}", err).into())]
                    ))
                } else {
                    attempts += 1;
                    thread::sleep(Duration::from_secs(10));
                }
            };
        };

        Ok((
            Some(MakerSwapCommand::ValidateTakerPayment),
            vec![MakerSwapEvent::TakerPaymentReceived(tx_details), MakerSwapEvent::TakerPaymentWaitConfirmStarted]
        ))
    }

    fn validate_taker_payment(&self) -> Result<(Option<MakerSwapCommand>, Vec<MakerSwapEvent>), String> {
        let wait_duration = self.data.lock_duration / 3;
        let wait_taker_payment = self.data.started_at + wait_duration;

        let validated = self.taker_coin.validate_taker_payment(
            &unwrap!(self.taker_payment.clone()).tx_hex,
            self.taker_payment_lock as u32,
            &*self.other_persistent_pub,
            &*dhash160(&self.data.secret.0),
            self.taker_amount.clone(),
        );

        if let Err(e) = validated {
            return Ok((
                Some(MakerSwapCommand::RefundMakerPayment),
                vec![MakerSwapEvent::TakerPaymentValidateFailed(ERRL!("!taker_coin.validate_taker_payment: {}", e).into())]
            ))
        }

        let wait = self.taker_coin.wait_for_confirmations(
            &unwrap!(self.taker_payment.clone()).tx_hex,
            self.data.taker_payment_confirmations,
            wait_taker_payment,
            15,
        );

        if let Err(err) = wait {
            return Ok((
                Some(MakerSwapCommand::RefundMakerPayment),
                vec![MakerSwapEvent::TakerPaymentValidateFailed(ERRL!("!taker_coin.wait_for_confirmations: {}", err).into())]
            ))
        }

        Ok((
            Some(MakerSwapCommand::SpendTakerPayment),
            vec![MakerSwapEvent::TakerPaymentValidatedAndConfirmed]
        ))
    }

    fn spend_taker_payment(&self) -> Result<(Option<MakerSwapCommand>, Vec<MakerSwapEvent>), String> {
        let spend_fut = self.taker_coin.send_maker_spends_taker_payment(
            &unwrap!(self.taker_payment.clone()).tx_hex,
            self.taker_payment_lock as u32,
            &*self.other_persistent_pub,
            &self.data.secret.0,
        );

        let transaction = match spend_fut.wait() {
            Ok(t) => t,
            Err(err) => return Ok((
                Some(MakerSwapCommand::RefundMakerPayment),
                vec![MakerSwapEvent::TakerPaymentSpendFailed(ERRL!("!taker_coin.send_maker_spends_taker_payment: {}", err).into())]
            ))
        };

        let hash = transaction.tx_hash();
        log!({ "Taker payment spend tx {:02x}", hash });

        // we can attempt to get the details in loop here as transaction was already sent and
        // is present on blockchain so only transport errors are expected to happen
        let tx_details = loop {
            match self.taker_coin.tx_details_by_hash(&hash) {
                Ok(details) => break details,
                Err(e) => {
                    log!({"Error {} getting tx details of {:02x}", e, hash});
                    thread::sleep(Duration::from_secs(30));
                    continue;
                }
            }
        };
        Ok((
            Some(MakerSwapCommand::Finish),
            vec![MakerSwapEvent::TakerPaymentSpent(tx_details)]
        ))
    }

    fn refund_maker_payment(&self) -> Result<(Option<MakerSwapCommand>, Vec<MakerSwapEvent>), String> {
        // have to wait for 1 hour more due as some coins have BIP113 activated so these will reject transactions with locktime == present time
        // https://github.com/bitcoin/bitcoin/blob/master/doc/release-notes/release-notes-0.11.2.md#bip113-mempool-only-locktime-enforcement-using-getmediantimepast
        while now_ms() / 1000 < self.data.maker_payment_lock + 3700 {
            std::thread::sleep(Duration::from_secs(10));
        }

        let spend_fut = self.maker_coin.send_maker_refunds_payment(
            &unwrap!(self.maker_payment.clone()).tx_hex,
            self.data.maker_payment_lock as u32,
            &*self.other_persistent_pub,
            &*dhash160(&self.data.secret.0),
        );

        let transaction = match spend_fut.wait() {
            Ok(t) => t,
            Err(err) => return Ok((
                Some(MakerSwapCommand::Finish),
                vec![MakerSwapEvent::MakerPaymentRefundFailed(ERRL!("!maker_coin.send_maker_refunds_payment: {}", err).into())]
            ))
        };
        let hash = transaction.tx_hash();
        log!({ "Maker payment refund tx {:02x}", hash });

        // we can attempt to get the details in loop here as transaction was already sent and
        // is present on blockchain so only transport errors are expected to happen
        let tx_details = loop {
            match self.maker_coin.tx_details_by_hash(&hash) {
                Ok(details) => break details,
                Err(e) => {
                    log!({"Error {} getting tx details of {:02x}", e, hash});
                    thread::sleep(Duration::from_secs(30));
                    continue;
                }
            }
        };
        Ok((
            Some(MakerSwapCommand::Finish),
            vec![MakerSwapEvent::MakerPaymentRefunded(tx_details)],
        ))
    }

    pub fn load_from_saved(
        ctx: MmArc,
        maker_coin: MmCoinEnum,
        taker_coin: MmCoinEnum,
        saved: MakerSavedSwap
    ) -> Result<(Self, Option<MakerSwapCommand>), String> {
        if saved.events.is_empty() {
            return ERR!("Can't restore swap from empty events set");
        };

        match &saved.events[0].event {
            MakerSwapEvent::Started(data) => {
                let mut taker = bits256::from([0; 32]);
                taker.bytes = data.taker.0;
                let my_persistent_pub = H264::from(&**ctx.secp256k1_key_pair().public());

                let mut swap = MakerSwap::new(
                    ctx,
                    taker.into(),
                    maker_coin,
                    taker_coin,
                    data.maker_amount.clone(),
                    data.taker_amount.clone(),
                    my_persistent_pub,
                    saved.uuid,
                );
                let command = saved.events.last().unwrap().get_command();
                for saved_event in saved.events {
                    try_s!(swap.apply_event(saved_event.event));
                }
                Ok((swap, command))
            },
            _ => ERR!("First swap event must be Started"),
        }
    }

    pub fn recover_funds(&self) -> Result<RecoveredSwap, String> {
        if self.finished_at == 0 { return ERR!("Swap must be finished before recover funds attempt"); }

        if self.maker_payment_refund.is_some() { return ERR!("Maker payment is refunded, swap is not recoverable"); }

        if self.taker_payment_spend.is_some() { return ERR!("Taker payment is spent, swap is not recoverable"); }

        let maker_payment = match &self.maker_payment {
            Some(tx) => tx.tx_hex.0.clone(),
            None => {
                let maybe_maker_payment = try_s!(self.maker_coin.check_if_my_payment_sent(
                    self.data.maker_payment_lock as u32,
                    &*self.other_persistent_pub,
                    &*dhash160(&self.data.secret.0),
                    self.data.maker_coin_start_block,
                ));
                match maybe_maker_payment {
                    Some(tx) => tx.tx_hex(),
                    None => return ERR!("Maker payment transaction was not found"),
                }
            }
        };
        // validate that maker payment is not spent
        match self.maker_coin.search_for_swap_tx_spend_my(
            self.data.maker_payment_lock as u32,
            &*self.other_persistent_pub,
            &*dhash160(&self.data.secret.0),
            &maker_payment,
            self.data.maker_coin_start_block,
        ) {
            Ok(Some(FoundSwapTxSpend::Spent(tx))) => return ERR!("Maker payment was already spent by {} tx {:02x}", self.maker_coin.ticker(), tx.tx_hash()),
            Ok(Some(FoundSwapTxSpend::Refunded(tx))) => return ERR!("Maker payment was already refunded by {} tx {:02x}", self.maker_coin.ticker(), tx.tx_hash()),
            Err(e) => return ERR!("Error {} when trying to find maker payment spend", e),
            Ok(None) => (), // payment is not spent, continue
        }

        if now_ms() / 1000 < self.data.maker_payment_lock + 3700 {
            return ERR!("Too early to refund, wait until {}", self.data.maker_payment_lock + 3700);
        }
        let transaction = try_s!(self.maker_coin.send_maker_refunds_payment(
            &maker_payment,
            self.data.maker_payment_lock as u32,
            &*self.other_persistent_pub,
            &*dhash160(&self.data.secret.0),
        ).wait());

        Ok(RecoveredSwap {
            action: RecoveredSwapAction::RefundedMyPayment,
            coin: self.maker_coin.ticker().to_string(),
            transaction,
        })
    }
}

impl AtomicSwap for MakerSwap {
    fn locked_amount(&self) -> LockedAmount {
        // if maker payment is not sent yet the maker amount must be virtually locked
        let amount = match self.maker_payment {
            Some(_) => 0.into(),
            None => self.maker_amount.clone(),
        };

        LockedAmount {
            coin: self.maker_coin.ticker().to_string(),
            amount,
        }
    }

    fn uuid(&self) -> &str {
        &self.uuid
    }

    fn maker_coin(&self) -> &str { self.maker_coin.ticker() }

    fn taker_coin(&self) -> &str { self.taker_coin.ticker() }
}

pub enum MakerSwapCommand {
    Start,
    Negotiate,
    WaitForTakerFee,
    SendPayment,
    WaitForTakerPayment,
    ValidateTakerPayment,
    SpendTakerPayment,
    RefundMakerPayment,
    Finish
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum MakerSwapEvent {
    Started(MakerSwapData),
    StartFailed(SwapError),
    Negotiated(TakerNegotiationData),
    NegotiateFailed(SwapError),
    TakerFeeValidated(TransactionDetails),
    TakerFeeValidateFailed(SwapError),
    MakerPaymentSent(TransactionDetails),
    MakerPaymentTransactionFailed(SwapError),
    MakerPaymentDataSendFailed(SwapError),
    TakerPaymentReceived(TransactionDetails),
    TakerPaymentWaitConfirmStarted,
    TakerPaymentValidatedAndConfirmed,
    TakerPaymentValidateFailed(SwapError),
    TakerPaymentSpent(TransactionDetails),
    TakerPaymentSpendFailed(SwapError),
    MakerPaymentRefunded(TransactionDetails),
    MakerPaymentRefundFailed(SwapError),
    Finished,
}

impl MakerSwapEvent {
    fn status_str(&self) -> String {
        match self {
            MakerSwapEvent::Started(_) => "Started...".to_owned(),
            MakerSwapEvent::StartFailed(_) => "Start failed...".to_owned(),
            MakerSwapEvent::Negotiated(_) => "Negotiated...".to_owned(),
            MakerSwapEvent::NegotiateFailed(_) => "Negotiate failed...".to_owned(),
            MakerSwapEvent::TakerFeeValidated(_) => "Taker fee validated...".to_owned(),
            MakerSwapEvent::TakerFeeValidateFailed(_) => "Taker fee validate failed...".to_owned(),
            MakerSwapEvent::MakerPaymentSent(_) => "Maker payment sent...".to_owned(),
            MakerSwapEvent::MakerPaymentTransactionFailed(_) => "Maker payment failed...".to_owned(),
            MakerSwapEvent::MakerPaymentDataSendFailed(_) => "Maker payment failed...".to_owned(),
            MakerSwapEvent::TakerPaymentReceived(_) => "Taker payment received...".to_owned(),
            MakerSwapEvent::TakerPaymentWaitConfirmStarted => "Taker payment wait confirm started...".to_owned(),
            MakerSwapEvent::TakerPaymentValidatedAndConfirmed => "Taker payment validated and confirmed...".to_owned(),
            MakerSwapEvent::TakerPaymentValidateFailed(_) => "Taker payment validate failed...".to_owned(),
            MakerSwapEvent::TakerPaymentSpent(_) => "Taker payment spent...".to_owned(),
            MakerSwapEvent::TakerPaymentSpendFailed(_) => "Taker payment spend failed...".to_owned(),
            MakerSwapEvent::MakerPaymentRefunded(_) => "Maker payment refunded...".to_owned(),
            MakerSwapEvent::MakerPaymentRefundFailed(_) => "Maker payment refund failed...".to_owned(),
            MakerSwapEvent::Finished => "Finished".to_owned(),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct MakerSavedEvent {
    timestamp: u64,
    event: MakerSwapEvent,
}

impl MakerSavedEvent {
    /// next command that must be executed after swap is restored
    fn get_command(&self) -> Option<MakerSwapCommand> {
        match self.event {
            MakerSwapEvent::Started(_) => Some(MakerSwapCommand::Negotiate),
            MakerSwapEvent::StartFailed(_) => Some(MakerSwapCommand::Finish),
            MakerSwapEvent::Negotiated(_) => Some(MakerSwapCommand::WaitForTakerFee),
            MakerSwapEvent::NegotiateFailed(_) => Some(MakerSwapCommand::Finish),
            MakerSwapEvent::TakerFeeValidated(_) => Some(MakerSwapCommand::SendPayment),
            MakerSwapEvent::TakerFeeValidateFailed(_) => Some(MakerSwapCommand::Finish),
            MakerSwapEvent::MakerPaymentSent(_) => Some(MakerSwapCommand::WaitForTakerPayment),
            MakerSwapEvent::MakerPaymentTransactionFailed(_) => Some(MakerSwapCommand::Finish),
            MakerSwapEvent::MakerPaymentDataSendFailed(_) => Some(MakerSwapCommand::RefundMakerPayment),
            MakerSwapEvent::TakerPaymentReceived(_) => Some(MakerSwapCommand::ValidateTakerPayment),
            MakerSwapEvent::TakerPaymentWaitConfirmStarted => Some(MakerSwapCommand::ValidateTakerPayment),
            MakerSwapEvent::TakerPaymentValidatedAndConfirmed => Some(MakerSwapCommand::SpendTakerPayment),
            MakerSwapEvent::TakerPaymentValidateFailed(_) => Some(MakerSwapCommand::RefundMakerPayment),
            MakerSwapEvent::TakerPaymentSpent(_) => Some(MakerSwapCommand::Finish),
            MakerSwapEvent::TakerPaymentSpendFailed(_) => Some(MakerSwapCommand::RefundMakerPayment),
            MakerSwapEvent::MakerPaymentRefunded(_) => Some(MakerSwapCommand::Finish),
            MakerSwapEvent::MakerPaymentRefundFailed(_) => Some(MakerSwapCommand::Finish),
            MakerSwapEvent::Finished => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MakerSavedSwap {
    pub uuid: String,
    events: Vec<MakerSavedEvent>,
    maker_amount: Option<BigDecimal>,
    maker_coin: Option<String>,
    taker_amount: Option<BigDecimal>,
    taker_coin: Option<String>,
    gui: Option<String>,
    mm_version: Option<String>,
    success_events: Vec<String>,
    error_events: Vec<String>,
}

impl MakerSavedSwap {
    pub fn maker_coin(&self) -> Result<String, String> {
        match self.events.first() {
            Some(event) => match &event.event {
                MakerSwapEvent::Started(data) => Ok(data.maker_coin.clone()),
                _ => ERR!("First swap event must be Started"),
            },
            None => ERR!("Can't get maker coin, events are empty"),
        }
    }

    pub fn taker_coin(&self) -> Result<String, String> {
        match self.events.first() {
            Some(event) => match &event.event {
                MakerSwapEvent::Started(data) => Ok(data.taker_coin.clone()),
                _ => ERR!("First swap event must be Started"),
            },
            None => ERR!("Can't get maker coin, events are empty"),
        }
    }

    pub fn is_finished(&self) -> bool {
        match self.events.last() {
            Some(event) => event.event == MakerSwapEvent::Finished,
            None => false,
        }
    }

    pub fn get_my_info(&self) -> Option<MySwapInfo> {
        match self.events.first() {
            Some(event) => match &event.event {
                MakerSwapEvent::Started(data) => {
                    Some(MySwapInfo {
                        my_coin: data.maker_coin.clone(),
                        other_coin: data.taker_coin.clone(),
                        my_amount: data.maker_amount.clone(),
                        other_amount: data.taker_amount.clone(),
                        started_at: data.started_at,
                    })
                },
                _ => None,
            },
            None => None,
        }
    }

    pub fn hide_secret(&mut self) {
        match self.events.first_mut() {
            Some(ref mut event) => match &mut event.event {
                MakerSwapEvent::Started(ref mut data) => data.secret = H256Json::default(),
                _ => (),
            }
            None => (),
        }
    }

    pub fn is_recoverable(&self) -> bool {
        if !self.is_finished() { return false };
        for event in self.events.iter() {
            match event.event {
                MakerSwapEvent::StartFailed(_) | MakerSwapEvent::NegotiateFailed(_) | MakerSwapEvent::TakerFeeValidateFailed(_) |
                MakerSwapEvent::TakerPaymentSpent(_) | MakerSwapEvent::MakerPaymentRefunded(_) => {
                    return false;
                }
                _ => (),
            }
        }
        true
    }
}

/// Starts the maker swap and drives it to completion (until None next command received).
/// Panics in case of command or event apply fails, not sure yet how to handle such situations
/// because it's usually means that swap is in invalid state which is possible only if there's developer error.
/// Every produced event is saved to local DB. Swap status is broadcasted to P2P network after completion.
pub fn run_maker_swap(swap: MakerSwap, initial_command: Option<MakerSwapCommand>) {
    let mut command = initial_command.unwrap_or(MakerSwapCommand::Start);
    let mut events;
    let ctx = swap.ctx.clone();
    let mut status = ctx.log.status_handle();
    let uuid = swap.uuid.clone();
    let swap_tags: &[&dyn TagParam] = &[&"swap", &("uuid", &uuid[..])];
    let running_swap = Arc::new(RwLock::new(swap));
    let weak_ref = Arc::downgrade(&running_swap);
    let swap_ctx = unwrap!(SwapsContext::from_ctx(&ctx));
    unwrap!(swap_ctx.running_swaps.lock()).push(weak_ref);

    loop {
        let res = unwrap!(unwrap!(running_swap.read()).handle_command(command));
        events = res.1;
        for event in events {
            let to_save = MakerSavedEvent {
                timestamp: now_ms(),
                event: event.clone(),
            };
            unwrap!(save_my_maker_swap_event(&ctx, &unwrap!(running_swap.read()), to_save));
            status.status(swap_tags, &event.status_str());
            unwrap!(running_swap.write().unwrap().apply_event(event));
        }
        match res.0 {
            Some(c) => { command = c; },
            None => {
                if let Err(e) = broadcast_my_swap_status(&uuid, &ctx) {
                    log!("!broadcast_my_swap_status(" (uuid) "): " (e));
                }
                break;
            },
        }
    }
}

#[cfg(test)]
mod maker_swap_tests {
    use coins::{MarketCoinOps, SwapOps, TestCoin};
    use coins::eth::{signed_eth_tx_from_bytes, SignedEthTx};
    use common::privkey::key_pair_from_seed;
    use common::mm_ctx::MmCtxBuilder;
    use mocktopus::mocking::*;
    use super::*;

    fn eth_tx_for_test() -> SignedEthTx {
        // raw transaction bytes of https://etherscan.io/tx/0x0869be3e5d4456a29d488a533ad6c118620fef450f36778aecf31d356ff8b41f
        let tx_bytes = [248, 240, 3, 133, 1, 42, 5, 242, 0, 131, 2, 73, 240, 148, 133, 0, 175, 192, 188, 82, 20, 114, 128, 130, 22, 51, 38, 194, 255, 12, 115, 244, 168, 113, 135, 110, 205, 245, 24, 127, 34, 254, 184, 132, 21, 44, 243, 175, 73, 33, 143, 82, 117, 16, 110, 27, 133, 82, 200, 114, 233, 42, 140, 198, 35, 21, 201, 249, 187, 180, 20, 46, 148, 40, 9, 228, 193, 130, 71, 199, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 152, 41, 132, 9, 201, 73, 19, 94, 237, 137, 35, 61, 4, 194, 207, 239, 152, 75, 175, 245, 157, 174, 10, 214, 161, 207, 67, 70, 87, 246, 231, 212, 47, 216, 119, 68, 237, 197, 125, 141, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 93, 72, 125, 102, 28, 159, 180, 237, 198, 97, 87, 80, 82, 200, 104, 40, 245, 221, 7, 28, 122, 104, 91, 99, 1, 159, 140, 25, 131, 101, 74, 87, 50, 168, 146, 187, 90, 160, 51, 1, 123, 247, 6, 108, 165, 181, 188, 40, 56, 47, 211, 229, 221, 73, 5, 15, 89, 81, 117, 225, 216, 108, 98, 226, 119, 232, 94, 184, 42, 106];
        unwrap!(signed_eth_tx_from_bytes(&tx_bytes))
    }

    #[test]
    fn test_recover_funds_maker_swap_payment_errored_but_sent() {
        // the swap ends up with MakerPaymentTransactionFailed error but the transaction is actually
        // sent, need to find it and refund
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"3.54932734","maker_coin":"KMD","maker_coin_start_block":1452970,"maker_payment_confirmations":1,"maker_payment_lock":1563759539,"my_persistent_pub":"031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8","secret":"0000000000000000000000000000000000000000000000000000000000000000","started_at":1563743939,"taker":"101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9","taker_amount":"0.02004833998671660000000000","taker_coin":"ETH","taker_coin_start_block":8196380,"taker_payment_confirmations":1,"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"},"type":"Started"},"timestamp":1563743939211},{"event":{"data":{"taker_payment_locktime":1563751737,"taker_pubkey":"03101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9"},"type":"Negotiated"},"timestamp":1563743979835},{"event":{"data":{"block_height":8196386,"coin":"ETH","fee_details":null,"from":["0x3D6a2f4Dd6085b34EeD6cBc2D3aaABd0D3B697C1"],"internal_id":"00","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":1563744052,"to":["0xD8997941Dd1346e9231118D5685d866294f59e5b"],"total_amount":0.0001,"tx_hash":"a59203eb2328827de00bed699a29389792906e4f39fdea145eb40dc6b3821bd6","tx_hex":"f8690284ee6b280082520894d8997941dd1346e9231118d5685d866294f59e5b865af3107a4000801ca0743d2b7c9fad65805d882179062012261be328d7628ae12ee08eff8d7657d993a07eecbd051f49d35279416778faa4664962726d516ce65e18755c9b9406a9c2fd"},"type":"TakerFeeValidated"},"timestamp":1563744052878},{"event":{"data":{"error":"lp_swap:1888] eth:654] RPC error: Error { code: ServerError(-32010), message: \"Transaction with the same hash was already imported.\", data: None }"},"type":"MakerPaymentTransactionFailed"},"timestamp":1563744118577},{"event":{"type":"Finished"},"timestamp":1563763243350}],"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        static mut MY_PAYMENT_SENT_CALLED: bool = false;
        TestCoin::check_if_my_payment_sent.mock_safe(|_, _, _, _, _| {
            unsafe { MY_PAYMENT_SENT_CALLED = true };
            MockResult::Return(Ok(Some(eth_tx_for_test().into())))
        });

        static mut MAKER_REFUND_CALLED: bool = false;
        TestCoin::send_maker_refunds_payment.mock_safe(|_, _, _, _, _| {
            unsafe { MAKER_REFUND_CALLED = true };
            MockResult::Return(Box::new(futures01::future::ok(eth_tx_for_test().into())))
        });
        TestCoin::search_for_swap_tx_spend_my.mock_safe(|_, _, _, _, _, _| MockResult::Return(Ok(None)));
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, maker_saved_swap));
        let actual = unwrap!(maker_swap.recover_funds());
        let expected = RecoveredSwap {
            action: RecoveredSwapAction::RefundedMyPayment,
            coin: "ticker".to_string(),
            transaction: eth_tx_for_test().into(),
        };
        assert_eq!(expected, actual);
        assert!(unsafe { MY_PAYMENT_SENT_CALLED });
        assert!(unsafe { MAKER_REFUND_CALLED });
    }

    #[test]
    fn test_recover_funds_maker_payment_refund_errored() {
        // the swap ends up with MakerPaymentRefundFailed error
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"0.58610590","maker_coin":"KMD","maker_coin_start_block":1450923,"maker_payment_confirmations":1,"maker_payment_lock":1563636475,"my_persistent_pub":"031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8","secret":"0000000000000000000000000000000000000000000000000000000000000000","started_at":1563620875,"taker":"14a96292bfcd7762ece8eb08ead915da927c2619277363853572f30880d5155e","taker_amount":"0.0077700000552410000000000","taker_coin":"LTC","taker_coin_start_block":1670837,"taker_payment_confirmations":1,"uuid":"9db641f5-4300-4527-9fa6-f1c391d42c35"},"type":"Started"},"timestamp":1563620875062},{"event":{"data":{"taker_payment_locktime":1563628675,"taker_pubkey":"02713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91"},"type":"Negotiated"},"timestamp":1563620915497},{"event":{"data":{"block_height":0,"coin":"LTC","fee_details":{"amount":0.001},"from":["LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD"],"internal_id":"6740136eaaa615d9d231969e3a9599d0fc59e53989237a8d31cd6fc86c160013","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":0,"to":["LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD","LdeeicEe3dYpjy36TPWrufiGToyaaEP2Zs"],"total_amount":0.0179204,"tx_hash":"6740136eaaa615d9d231969e3a9599d0fc59e53989237a8d31cd6fc86c160013","tx_hex":"0100000001a2586ea8294cedc55741bef625ba72c646399903391a7f6c604a58c6263135f2000000006b4830450221009c78c8ba4a7accab6b09f9a95da5bc59c81f4fc1e60b288ec3c5462b4d02ef01022056b63be1629cf17751d3cc5ffec51bcb1d7f9396e9ce9ca254d0f34104f7263a012102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ffffffff0210270000000000001976a914ca1e04745e8ca0c60d8c5881531d51bec470743f88ac78aa1900000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac5bf6325d"},"type":"TakerFeeValidated"},"timestamp":1563620976060},{"event":{"data":{"block_height":0,"coin":"KMD","fee_details":{"amount":1e-05},"from":["RT9MpMyucqXiX8bZLimXBnrrn2ofmdGNKd"],"internal_id":"d0f6e664cea9d89fe7b5cf8005fdca070d1ab1d05a482aaef95c08cdaecddf0a","my_balance_change":-0.5861159,"received_by_me":0.41387409,"spent_by_me":0.99998999,"timestamp":0,"to":["RT9MpMyucqXiX8bZLimXBnrrn2ofmdGNKd","bLVo4svJDxUF6C2fVivmV91HJqVjrkkAf4"],"total_amount":0.99998999,"tx_hash":"d0f6e664cea9d89fe7b5cf8005fdca070d1ab1d05a482aaef95c08cdaecddf0a","tx_hex":"0400008085202f89019f1cbda354342cdf982046b331bbd3791f53b692efc6e4becc36be495b2977d9000000006b483045022100fa9d4557394141f6a8b9bfb8cd594a521fd8bcd1965dbf8bc4e04abc849ac66e0220589f521814c10a7561abfd5e432f7a2ee60d4875fe4604618af3207dae531ac00121031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ffffffff029e537e030000000017a9145534898009f1467191065f6890b96914b39a1c018791857702000000001976a914c3f710deb7320b0efa6edb14e3ebeeb9155fa90d88ac72ee325d000000000000000000000000000000"},"type":"MakerPaymentSent"},"timestamp":1563620976189},{"event":{"data":{"block_height":0,"coin":"LTC","fee_details":{"amount":0.001},"from":["LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD"],"internal_id":"1e883eb2f3991e84ba27f53651f89b7dda708678a5b9813d043577f222b9ca30","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":0,"to":["3DgMcEEjxwXfnEVapgQSCBVy2tz9X41RmR","LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD"],"total_amount":0.0168204,"tx_hash":"1e883eb2f3991e84ba27f53651f89b7dda708678a5b9813d043577f222b9ca30","tx_hex":"01000000011300166cc86fcd318d7a238939e559fcd099953a9e9631d2d915a6aa6e134067010000006a47304402206781d5f2db2ff13d2ec7e266f774ea5630cc2dba4019e18e9716131b8b026051022006ebb33857b6d180f13aa6be2fc532f9734abde9d00ae14757e7d7ba3741c08c012102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ffffffff0228db0b000000000017a91483818667161bf94adda3964a81a231cbf6f5338187b0480c00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac7cf7325d"},"type":"TakerPaymentReceived"},"timestamp":1563621268320},{"event":{"type":"TakerPaymentWaitConfirmStarted"},"timestamp":1563621268321},{"event":{"type":"TakerPaymentValidatedAndConfirmed"},"timestamp":1563621778471},{"event":{"data":{"error":"lp_swap:2025] utxo:938] rpc_clients:719] JsonRpcError { request: JsonRpcRequest { jsonrpc: \"2.0\", id: \"9\", method: \"blockchain.transaction.broadcast\", params: [String(\"010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d\")] }, error: Response(Object({\"code\": Number(1), \"message\": String(\"the transaction was rejected by network rules.\\n\\nMissing inputs\\n[010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d]\")})) }"},"type":"TakerPaymentSpendFailed"},"timestamp":1563638060583},{"event":{"data":{"error":"lp_swap:2025] utxo:938] rpc_clients:719] JsonRpcError { request: JsonRpcRequest { jsonrpc: \"2.0\", id: \"9\", method: \"blockchain.transaction.broadcast\", params: [String(\"010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d\")] }, error: Response(Object({\"code\": Number(1), \"message\": String(\"the transaction was rejected by network rules.\\n\\nMissing inputs\\n[010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d]\")})) }"},"type":"MakerPaymentRefundFailed"},"timestamp":1563638060583},{"event":{"type":"Finished"},"timestamp":1563621778483}],"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"uuid":"9db641f5-4300-4527-9fa6-f1c391d42c35"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        static mut MAKER_REFUND_CALLED: bool = false;

        TestCoin::send_maker_refunds_payment.mock_safe(|_, _, _, _, _| {
            unsafe { MAKER_REFUND_CALLED = true };
            MockResult::Return(Box::new(futures01::future::ok(eth_tx_for_test().into())))
        });

        TestCoin::search_for_swap_tx_spend_my.mock_safe(|_, _, _, _, _, _| MockResult::Return(Ok(None)));
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, maker_saved_swap));
        let actual = unwrap!(maker_swap.recover_funds());
        let expected = RecoveredSwap {
            action: RecoveredSwapAction::RefundedMyPayment,
            coin: "ticker".to_string(),
            transaction: eth_tx_for_test().into(),
        };
        assert_eq!(expected, actual);
        assert!(unsafe { MAKER_REFUND_CALLED });
    }

    #[test]
    fn test_recover_funds_maker_payment_refund_errored_already_refunded() {
        // the swap ends up with MakerPaymentRefundFailed error
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"0.58610590","maker_coin":"KMD","maker_coin_start_block":1450923,"maker_payment_confirmations":1,"maker_payment_lock":1563636475,"my_persistent_pub":"031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8","secret":"0000000000000000000000000000000000000000000000000000000000000000","started_at":1563620875,"taker":"14a96292bfcd7762ece8eb08ead915da927c2619277363853572f30880d5155e","taker_amount":"0.0077700000552410000000000","taker_coin":"LTC","taker_coin_start_block":1670837,"taker_payment_confirmations":1,"uuid":"9db641f5-4300-4527-9fa6-f1c391d42c35"},"type":"Started"},"timestamp":1563620875062},{"event":{"data":{"taker_payment_locktime":1563628675,"taker_pubkey":"02713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91"},"type":"Negotiated"},"timestamp":1563620915497},{"event":{"data":{"block_height":0,"coin":"LTC","fee_details":{"amount":0.001},"from":["LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD"],"internal_id":"6740136eaaa615d9d231969e3a9599d0fc59e53989237a8d31cd6fc86c160013","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":0,"to":["LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD","LdeeicEe3dYpjy36TPWrufiGToyaaEP2Zs"],"total_amount":0.0179204,"tx_hash":"6740136eaaa615d9d231969e3a9599d0fc59e53989237a8d31cd6fc86c160013","tx_hex":"0100000001a2586ea8294cedc55741bef625ba72c646399903391a7f6c604a58c6263135f2000000006b4830450221009c78c8ba4a7accab6b09f9a95da5bc59c81f4fc1e60b288ec3c5462b4d02ef01022056b63be1629cf17751d3cc5ffec51bcb1d7f9396e9ce9ca254d0f34104f7263a012102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ffffffff0210270000000000001976a914ca1e04745e8ca0c60d8c5881531d51bec470743f88ac78aa1900000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac5bf6325d"},"type":"TakerFeeValidated"},"timestamp":1563620976060},{"event":{"data":{"block_height":0,"coin":"KMD","fee_details":{"amount":1e-05},"from":["RT9MpMyucqXiX8bZLimXBnrrn2ofmdGNKd"],"internal_id":"d0f6e664cea9d89fe7b5cf8005fdca070d1ab1d05a482aaef95c08cdaecddf0a","my_balance_change":-0.5861159,"received_by_me":0.41387409,"spent_by_me":0.99998999,"timestamp":0,"to":["RT9MpMyucqXiX8bZLimXBnrrn2ofmdGNKd","bLVo4svJDxUF6C2fVivmV91HJqVjrkkAf4"],"total_amount":0.99998999,"tx_hash":"d0f6e664cea9d89fe7b5cf8005fdca070d1ab1d05a482aaef95c08cdaecddf0a","tx_hex":"0400008085202f89019f1cbda354342cdf982046b331bbd3791f53b692efc6e4becc36be495b2977d9000000006b483045022100fa9d4557394141f6a8b9bfb8cd594a521fd8bcd1965dbf8bc4e04abc849ac66e0220589f521814c10a7561abfd5e432f7a2ee60d4875fe4604618af3207dae531ac00121031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ffffffff029e537e030000000017a9145534898009f1467191065f6890b96914b39a1c018791857702000000001976a914c3f710deb7320b0efa6edb14e3ebeeb9155fa90d88ac72ee325d000000000000000000000000000000"},"type":"MakerPaymentSent"},"timestamp":1563620976189},{"event":{"data":{"block_height":0,"coin":"LTC","fee_details":{"amount":0.001},"from":["LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD"],"internal_id":"1e883eb2f3991e84ba27f53651f89b7dda708678a5b9813d043577f222b9ca30","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":0,"to":["3DgMcEEjxwXfnEVapgQSCBVy2tz9X41RmR","LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD"],"total_amount":0.0168204,"tx_hash":"1e883eb2f3991e84ba27f53651f89b7dda708678a5b9813d043577f222b9ca30","tx_hex":"01000000011300166cc86fcd318d7a238939e559fcd099953a9e9631d2d915a6aa6e134067010000006a47304402206781d5f2db2ff13d2ec7e266f774ea5630cc2dba4019e18e9716131b8b026051022006ebb33857b6d180f13aa6be2fc532f9734abde9d00ae14757e7d7ba3741c08c012102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ffffffff0228db0b000000000017a91483818667161bf94adda3964a81a231cbf6f5338187b0480c00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac7cf7325d"},"type":"TakerPaymentReceived"},"timestamp":1563621268320},{"event":{"type":"TakerPaymentWaitConfirmStarted"},"timestamp":1563621268321},{"event":{"type":"TakerPaymentValidatedAndConfirmed"},"timestamp":1563621778471},{"event":{"data":{"error":"lp_swap:2025] utxo:938] rpc_clients:719] JsonRpcError { request: JsonRpcRequest { jsonrpc: \"2.0\", id: \"9\", method: \"blockchain.transaction.broadcast\", params: [String(\"010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d\")] }, error: Response(Object({\"code\": Number(1), \"message\": String(\"the transaction was rejected by network rules.\\n\\nMissing inputs\\n[010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d]\")})) }"},"type":"TakerPaymentSpendFailed"},"timestamp":1563638060583},{"event":{"data":{"error":"lp_swap:2025] utxo:938] rpc_clients:719] JsonRpcError { request: JsonRpcRequest { jsonrpc: \"2.0\", id: \"9\", method: \"blockchain.transaction.broadcast\", params: [String(\"010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d\")] }, error: Response(Object({\"code\": Number(1), \"message\": String(\"the transaction was rejected by network rules.\\n\\nMissing inputs\\n[010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d]\")})) }"},"type":"MakerPaymentRefundFailed"},"timestamp":1563638060583},{"event":{"type":"Finished"},"timestamp":1563621778483}],"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"uuid":"9db641f5-4300-4527-9fa6-f1c391d42c35"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        TestCoin::search_for_swap_tx_spend_my.mock_safe(|_, _, _, _, _, _|
            MockResult::Return(Ok(Some(FoundSwapTxSpend::Refunded(eth_tx_for_test().into()))))
        );
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, maker_saved_swap));
        assert!(maker_swap.recover_funds().is_err());
    }

    #[test]
    fn test_recover_funds_maker_payment_refund_errored_already_spent() {
        // the swap ends up with MakerPaymentRefundFailed error
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"0.58610590","maker_coin":"KMD","maker_coin_start_block":1450923,"maker_payment_confirmations":1,"maker_payment_lock":1563636475,"my_persistent_pub":"031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8","secret":"0000000000000000000000000000000000000000000000000000000000000000","started_at":1563620875,"taker":"14a96292bfcd7762ece8eb08ead915da927c2619277363853572f30880d5155e","taker_amount":"0.0077700000552410000000000","taker_coin":"LTC","taker_coin_start_block":1670837,"taker_payment_confirmations":1,"uuid":"9db641f5-4300-4527-9fa6-f1c391d42c35"},"type":"Started"},"timestamp":1563620875062},{"event":{"data":{"taker_payment_locktime":1563628675,"taker_pubkey":"02713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91"},"type":"Negotiated"},"timestamp":1563620915497},{"event":{"data":{"block_height":0,"coin":"LTC","fee_details":{"amount":0.001},"from":["LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD"],"internal_id":"6740136eaaa615d9d231969e3a9599d0fc59e53989237a8d31cd6fc86c160013","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":0,"to":["LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD","LdeeicEe3dYpjy36TPWrufiGToyaaEP2Zs"],"total_amount":0.0179204,"tx_hash":"6740136eaaa615d9d231969e3a9599d0fc59e53989237a8d31cd6fc86c160013","tx_hex":"0100000001a2586ea8294cedc55741bef625ba72c646399903391a7f6c604a58c6263135f2000000006b4830450221009c78c8ba4a7accab6b09f9a95da5bc59c81f4fc1e60b288ec3c5462b4d02ef01022056b63be1629cf17751d3cc5ffec51bcb1d7f9396e9ce9ca254d0f34104f7263a012102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ffffffff0210270000000000001976a914ca1e04745e8ca0c60d8c5881531d51bec470743f88ac78aa1900000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac5bf6325d"},"type":"TakerFeeValidated"},"timestamp":1563620976060},{"event":{"data":{"block_height":0,"coin":"KMD","fee_details":{"amount":1e-05},"from":["RT9MpMyucqXiX8bZLimXBnrrn2ofmdGNKd"],"internal_id":"d0f6e664cea9d89fe7b5cf8005fdca070d1ab1d05a482aaef95c08cdaecddf0a","my_balance_change":-0.5861159,"received_by_me":0.41387409,"spent_by_me":0.99998999,"timestamp":0,"to":["RT9MpMyucqXiX8bZLimXBnrrn2ofmdGNKd","bLVo4svJDxUF6C2fVivmV91HJqVjrkkAf4"],"total_amount":0.99998999,"tx_hash":"d0f6e664cea9d89fe7b5cf8005fdca070d1ab1d05a482aaef95c08cdaecddf0a","tx_hex":"0400008085202f89019f1cbda354342cdf982046b331bbd3791f53b692efc6e4becc36be495b2977d9000000006b483045022100fa9d4557394141f6a8b9bfb8cd594a521fd8bcd1965dbf8bc4e04abc849ac66e0220589f521814c10a7561abfd5e432f7a2ee60d4875fe4604618af3207dae531ac00121031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ffffffff029e537e030000000017a9145534898009f1467191065f6890b96914b39a1c018791857702000000001976a914c3f710deb7320b0efa6edb14e3ebeeb9155fa90d88ac72ee325d000000000000000000000000000000"},"type":"MakerPaymentSent"},"timestamp":1563620976189},{"event":{"data":{"block_height":0,"coin":"LTC","fee_details":{"amount":0.001},"from":["LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD"],"internal_id":"1e883eb2f3991e84ba27f53651f89b7dda708678a5b9813d043577f222b9ca30","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":0,"to":["3DgMcEEjxwXfnEVapgQSCBVy2tz9X41RmR","LKquWDGkJHEcFn85Dzw4FV5XwYp8GT3WvD"],"total_amount":0.0168204,"tx_hash":"1e883eb2f3991e84ba27f53651f89b7dda708678a5b9813d043577f222b9ca30","tx_hex":"01000000011300166cc86fcd318d7a238939e559fcd099953a9e9631d2d915a6aa6e134067010000006a47304402206781d5f2db2ff13d2ec7e266f774ea5630cc2dba4019e18e9716131b8b026051022006ebb33857b6d180f13aa6be2fc532f9734abde9d00ae14757e7d7ba3741c08c012102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ffffffff0228db0b000000000017a91483818667161bf94adda3964a81a231cbf6f5338187b0480c00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac7cf7325d"},"type":"TakerPaymentReceived"},"timestamp":1563621268320},{"event":{"type":"TakerPaymentWaitConfirmStarted"},"timestamp":1563621268321},{"event":{"type":"TakerPaymentValidatedAndConfirmed"},"timestamp":1563621778471},{"event":{"data":{"error":"lp_swap:2025] utxo:938] rpc_clients:719] JsonRpcError { request: JsonRpcRequest { jsonrpc: \"2.0\", id: \"9\", method: \"blockchain.transaction.broadcast\", params: [String(\"010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d\")] }, error: Response(Object({\"code\": Number(1), \"message\": String(\"the transaction was rejected by network rules.\\n\\nMissing inputs\\n[010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d]\")})) }"},"type":"TakerPaymentSpendFailed"},"timestamp":1563638060583},{"event":{"data":{"error":"lp_swap:2025] utxo:938] rpc_clients:719] JsonRpcError { request: JsonRpcRequest { jsonrpc: \"2.0\", id: \"9\", method: \"blockchain.transaction.broadcast\", params: [String(\"010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d\")] }, error: Response(Object({\"code\": Number(1), \"message\": String(\"the transaction was rejected by network rules.\\n\\nMissing inputs\\n[010000000130cab922f27735043d81b9a5788670da7d9bf85136f527ba841e99f3b23e881e00000000b6473044022058a0c1da6bcf8c1418899ff8475f3ab6dddbff918528451c1fe71c2f7dad176302204c2e0bcf8f9b5f09e02ccfeb9256e9b34fb355ea655a5704a8a3fa920079b91501514c6b63048314335db1752102713015d3fa4d30259e90be5f131beb593bf0131f3af2dcdb304e3322d8d52b91ac6782012088a9147ed38daab6085c1a1e4426e61dc87a3c2c081a958821031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8ac68feffffff0188540a00000000001976a91406ccabfd5f9075ecd5e8d0d31c0e973a54d51e8288ac1c2b335d]\")})) }"},"type":"MakerPaymentRefundFailed"},"timestamp":1563638060583},{"event":{"type":"Finished"},"timestamp":1563621778483}],"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"uuid":"9db641f5-4300-4527-9fa6-f1c391d42c35"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        TestCoin::search_for_swap_tx_spend_my.mock_safe(|_, _, _, _, _, _|
            MockResult::Return(Ok(Some(FoundSwapTxSpend::Spent(eth_tx_for_test().into()))))
        );
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, maker_saved_swap));
        assert!(maker_swap.recover_funds().is_err());
    }

    #[test]
    fn test_recover_funds_maker_swap_payment_errored_but_too_early_to_refund() {
        // the swap ends up with MakerPaymentTransactionFailed error but the transaction is actually
        // sent, need to find it and refund, prevent refund if payment is not spendable due to locktime restrictions
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"3.54932734","maker_coin":"KMD","maker_coin_start_block":1452970,"maker_payment_confirmations":1,"maker_payment_lock":1563759539,"my_persistent_pub":"031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8","secret":"0000000000000000000000000000000000000000000000000000000000000000","started_at":1563743939,"taker":"101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9","taker_amount":"0.02004833998671660000000000","taker_coin":"ETH","taker_coin_start_block":8196380,"taker_payment_confirmations":1,"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"},"type":"Started"},"timestamp":1563743939211},{"event":{"data":{"taker_payment_locktime":1563751737,"taker_pubkey":"03101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9"},"type":"Negotiated"},"timestamp":1563743979835},{"event":{"data":{"block_height":8196386,"coin":"ETH","fee_details":null,"from":["0x3D6a2f4Dd6085b34EeD6cBc2D3aaABd0D3B697C1"],"internal_id":"00","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":1563744052,"to":["0xD8997941Dd1346e9231118D5685d866294f59e5b"],"total_amount":0.0001,"tx_hash":"a59203eb2328827de00bed699a29389792906e4f39fdea145eb40dc6b3821bd6","tx_hex":"f8690284ee6b280082520894d8997941dd1346e9231118d5685d866294f59e5b865af3107a4000801ca0743d2b7c9fad65805d882179062012261be328d7628ae12ee08eff8d7657d993a07eecbd051f49d35279416778faa4664962726d516ce65e18755c9b9406a9c2fd"},"type":"TakerFeeValidated"},"timestamp":1563744052878},{"event":{"data":{"error":"lp_swap:1888] eth:654] RPC error: Error { code: ServerError(-32010), message: \"Transaction with the same hash was already imported.\", data: None }"},"type":"MakerPaymentTransactionFailed"},"timestamp":1563744118577},{"event":{"type":"Finished"},"timestamp":1563763243350}],"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        static mut MY_PAYMENT_SENT_CALLED: bool = false;
        TestCoin::check_if_my_payment_sent.mock_safe(|_, _, _, _, _| {
            unsafe { MY_PAYMENT_SENT_CALLED = true };
            MockResult::Return(Ok(Some(eth_tx_for_test().into())))
        });
        TestCoin::search_for_swap_tx_spend_my.mock_safe(|_, _, _, _, _, _| MockResult::Return(Ok(None)));
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (mut maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, maker_saved_swap));
        maker_swap.data.maker_payment_lock = (now_ms() / 1000) - 3690;
        assert!(maker_swap.recover_funds().is_err());
        assert!(unsafe { MY_PAYMENT_SENT_CALLED });
    }

    #[test]
    fn test_recover_funds_maker_swap_payment_errored_and_not_sent() {
        // the swap ends up with MakerPaymentTransactionFailed error and transaction is not sent,
        // recover must return error in this case
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"3.54932734","maker_coin":"KMD","maker_coin_start_block":1452970,"maker_payment_confirmations":1,"maker_payment_lock":1563759539,"my_persistent_pub":"031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8","secret":"0000000000000000000000000000000000000000000000000000000000000000","started_at":1563743939,"taker":"101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9","taker_amount":"0.02004833998671660000000000","taker_coin":"ETH","taker_coin_start_block":8196380,"taker_payment_confirmations":1,"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"},"type":"Started"},"timestamp":1563743939211},{"event":{"data":{"taker_payment_locktime":1563751737,"taker_pubkey":"03101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9"},"type":"Negotiated"},"timestamp":1563743979835},{"event":{"data":{"block_height":8196386,"coin":"ETH","fee_details":null,"from":["0x3D6a2f4Dd6085b34EeD6cBc2D3aaABd0D3B697C1"],"internal_id":"00","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":1563744052,"to":["0xD8997941Dd1346e9231118D5685d866294f59e5b"],"total_amount":0.0001,"tx_hash":"a59203eb2328827de00bed699a29389792906e4f39fdea145eb40dc6b3821bd6","tx_hex":"f8690284ee6b280082520894d8997941dd1346e9231118d5685d866294f59e5b865af3107a4000801ca0743d2b7c9fad65805d882179062012261be328d7628ae12ee08eff8d7657d993a07eecbd051f49d35279416778faa4664962726d516ce65e18755c9b9406a9c2fd"},"type":"TakerFeeValidated"},"timestamp":1563744052878},{"event":{"data":{"error":"lp_swap:1888] eth:654] RPC error: Error { code: ServerError(-32010), message: \"Transaction with the same hash was already imported.\", data: None }"},"type":"MakerPaymentTransactionFailed"},"timestamp":1563744118577},{"event":{"type":"Finished"},"timestamp":1563763243350}],"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        static mut MY_PAYMENT_SENT_CALLED: bool = false;
        TestCoin::check_if_my_payment_sent.mock_safe(|_, _, _, _, _| {
            unsafe { MY_PAYMENT_SENT_CALLED = true };
            MockResult::Return(Ok(None))
        });
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, maker_saved_swap));
        assert!(maker_swap.recover_funds().is_err());
        assert!(unsafe { MY_PAYMENT_SENT_CALLED });
    }

    #[test]
    fn test_recover_funds_maker_swap_not_finished() {
        // return error if swap is not finished
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"3.54932734","maker_coin":"KMD","maker_coin_start_block":1452970,"maker_payment_confirmations":1,"maker_payment_lock":1563759539,"my_persistent_pub":"031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8","secret":"0000000000000000000000000000000000000000000000000000000000000000","started_at":1563743939,"taker":"101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9","taker_amount":"0.02004833998671660000000000","taker_coin":"ETH","taker_coin_start_block":8196380,"taker_payment_confirmations":1,"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"},"type":"Started"},"timestamp":1563743939211},{"event":{"data":{"taker_payment_locktime":1563751737,"taker_pubkey":"03101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9"},"type":"Negotiated"},"timestamp":1563743979835},{"event":{"data":{"block_height":8196386,"coin":"ETH","fee_details":null,"from":["0x3D6a2f4Dd6085b34EeD6cBc2D3aaABd0D3B697C1"],"internal_id":"00","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":1563744052,"to":["0xD8997941Dd1346e9231118D5685d866294f59e5b"],"total_amount":0.0001,"tx_hash":"a59203eb2328827de00bed699a29389792906e4f39fdea145eb40dc6b3821bd6","tx_hex":"f8690284ee6b280082520894d8997941dd1346e9231118d5685d866294f59e5b865af3107a4000801ca0743d2b7c9fad65805d882179062012261be328d7628ae12ee08eff8d7657d993a07eecbd051f49d35279416778faa4664962726d516ce65e18755c9b9406a9c2fd"},"type":"TakerFeeValidated"},"timestamp":1563744052878}],"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, maker_saved_swap));
        assert!(maker_swap.recover_funds().is_err());
    }

    #[test]
    fn test_recover_funds_maker_swap_taker_payment_spent() {
        // return error if taker payment was spent
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"1","maker_coin":"BEER","maker_coin_start_block":154892,"maker_payment_confirmations":1,"maker_payment_lock":1563444026,"my_persistent_pub":"02631dcf1d4b1b693aa8c2751afc68e4794b1e5996566cfc701a663f8b7bbbe640","secret":"e1c9bd12a83f810813dc078ac398069b63d56bf1e94657def995c43cd1975302","started_at":1563428426,"taker":"031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3","taker_amount":"1","taker_coin":"ETOMIC","taker_coin_start_block":150282,"taker_payment_confirmations":1,"uuid":"983ce732-62a8-4a44-b4ac-7e4271adc977"},"type":"Started"},"timestamp":1563428426510},{"event":{"data":{"taker_payment_locktime":1563436226,"taker_pubkey":"02031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3"},"type":"Negotiated"},"timestamp":1563428466880},{"event":{"data":{"block_height":150283,"coin":"ETOMIC","fee_details":{"amount":0.00001},"from":["R9o9xTocqr6CeEDGDH6mEYpwLoMz6jNjMW"],"internal_id":"32f5bec2106dd3778dc32e3d856398ed0fa10b71c688672906a4fa0345cc4135","my_balance_change":0.0,"received_by_me":0.0,"spent_by_me":0.0,"timestamp":1563428493,"to":["R9o9xTocqr6CeEDGDH6mEYpwLoMz6jNjMW","RThtXup6Zo7LZAi8kRWgjAyi1s4u6U9Cpf"],"total_amount":71.81977626,"tx_hash":"32f5bec2106dd3778dc32e3d856398ed0fa10b71c688672906a4fa0345cc4135","tx_hex":"0400008085202f89015ba9c8f0aec5b409bc824bcddc1a5a40148d4bd065c10169249e44ec44d62db2010000006a473044022050a213db7486e34871b9e7ef850845d55e0d53431350c16fa14fb60b81b1858302204f1042761f84e5f8d22948358b3c4103861adf5293d1d9e7f58f3b7491470b19012102031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3ffffffff02bcf60100000000001976a914ca1e04745e8ca0c60d8c5881531d51bec470743f88ac764d12ac010000001976a91405aab5342166f8594baf17a7d9bef5d56744332788ac8806305d000000000000000000000000000000"},"type":"TakerFeeValidated"},"timestamp":1563428507723},{"event":{"data":{"block_height":0,"coin":"BEER","fee_details":{"amount":0.00001},"from":["RJTYiYeJ8eVvJ53n2YbrVmxWNNMVZjDGLh"],"internal_id":"1619d10a51925d2f3d0ef92d81cb6449b77d5dbe1f3ef5e7ae6c8bc19080cb5a","my_balance_change":-1.00001,"received_by_me":8250.37174399,"spent_by_me":8251.37175399,"timestamp":0,"to":["RJTYiYeJ8eVvJ53n2YbrVmxWNNMVZjDGLh","bEDXdMNnweUgfuvkNyEkM5qLn2zZWrp6y5"],"total_amount":8251.37175399,"tx_hash":"1619d10a51925d2f3d0ef92d81cb6449b77d5dbe1f3ef5e7ae6c8bc19080cb5a","tx_hex":"0400008085202f890176ead03820bc0c4e92dba39b5d7e7a1e176b165f6cfc7a5e2c000ed62e8a8134010000006b48304502210086ca9a6ea5e787f4c3001c4ddb7b2f4732d8bb2642e9e43d0f39df4b736a4aa402206dbd17753f728d70c9631b6c2d1bba125745a5bc9be6112febf0e0c8ada786b1012102631dcf1d4b1b693aa8c2751afc68e4794b1e5996566cfc701a663f8b7bbbe640ffffffff0200e1f5050000000017a91410503cfea67f03f025c5e1eeb18524464adf77ee877f360c18c00000001976a91464ae8510aac9546d5e7704e31ce177451386455588ac9b06305d000000000000000000000000000000"},"type":"MakerPaymentSent"},"timestamp":1563428512925},{"event":{"data":{"block_height":150285,"coin":"ETOMIC","fee_details":{"amount":0.00001},"from":["R9o9xTocqr6CeEDGDH6mEYpwLoMz6jNjMW"],"internal_id":"ee8b904efdee0d3bf0215d14a236489cde0b0efa92f7fa49faaa5fd97ed38ac0","my_balance_change":0.0,"received_by_me":0.0,"spent_by_me":0.0,"timestamp":1563428548,"to":["R9o9xTocqr6CeEDGDH6mEYpwLoMz6jNjMW","bG6qRgxfXGeBjXsKGSAVMJ5qMZ6oGm6UtX"],"total_amount":71.81847926,"tx_hash":"ee8b904efdee0d3bf0215d14a236489cde0b0efa92f7fa49faaa5fd97ed38ac0","tx_hex":"0400008085202f89013541cc4503faa406296788c6710ba10fed9863853d2ec38d77d36d10c2bef532010000006b483045022100a32e290d3a047ad75a512f9fd581c561c5153aa1b6be2b36915a9dd452cd0d4102204d1838b3cd15698ab424d15651d50983f0196e59b0b34abaad9cb792c97b527a012102031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3ffffffff0200e1f5050000000017a91424fc6f967eaa2751adbeb42a97c3497fbd9ddcce878e681ca6010000001976a91405aab5342166f8594baf17a7d9bef5d56744332788acbf06305d000000000000000000000000000000"},"type":"TakerPaymentReceived"},"timestamp":1563428664418},{"event":{"type":"TakerPaymentWaitConfirmStarted"},"timestamp":1563428664420},{"event":{"type":"TakerPaymentValidatedAndConfirmed"},"timestamp":1563428664824},{"event":{"data":{"block_height":0,"coin":"ETOMIC","fee_details":{"amount":0.00001},"from":["bG6qRgxfXGeBjXsKGSAVMJ5qMZ6oGm6UtX"],"internal_id":"8b48d7452a2a1c6b1128aa83ab946e5a624037c5327b527b18c3dcadb404f139","my_balance_change":0.99999,"received_by_me":0.99999,"spent_by_me":0.0,"timestamp":0,"to":["RJTYiYeJ8eVvJ53n2YbrVmxWNNMVZjDGLh"],"total_amount":1.0,"tx_hash":"8b48d7452a2a1c6b1128aa83ab946e5a624037c5327b527b18c3dcadb404f139","tx_hex":"0400008085202f8901c08ad37ed95faafa49faf792fa0e0bde9c4836a2145d21f03b0deefd4e908bee00000000d747304402206ac1f2b5b856b86585b4d2147309e3a7ef9dd4c35ffd85a49c409a4acd11602902204be03e2114888fae460eaf99675bae0c834ff80be8531a5bd30ee14baf0a52e30120e1c9bd12a83f810813dc078ac398069b63d56bf1e94657def995c43cd1975302004c6b6304c224305db1752102031d4256c4bc9f99ac88bf3dba21773132281f65f9bf23a59928bce08961e2f3ac6782012088a9143501575fb9a12a689bb94adad33cc78c13b0688c882102631dcf1d4b1b693aa8c2751afc68e4794b1e5996566cfc701a663f8b7bbbe640ac68ffffffff0118ddf505000000001976a91464ae8510aac9546d5e7704e31ce177451386455588ac28f92f5d000000000000000000000000000000"},"type":"TakerPaymentSpent"},"timestamp":1563428666150},{"event":{"type":"Finished"},"timestamp":1563428666152}],"my_info":{"my_amount":"1","my_coin":"BEER","other_amount":"1","other_coin":"ETOMIC","started_at":1563428426},"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"type":"Maker","uuid":"983ce732-62a8-4a44-b4ac-7e4271adc977"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, maker_saved_swap));
        assert!(maker_swap.recover_funds().is_err());
    }

    #[test]
    fn test_recover_funds_maker_swap_maker_payment_refunded() {
        // return error if maker payment was refunded
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"9.38455187130897","maker_coin":"VRSC","maker_coin_start_block":604407,"maker_payment_confirmations":1,"maker_payment_lock":1564317372,"my_persistent_pub":"03c2e08e48e6541b3265ccd430c5ecec7efc7d0d9fc4e310a9b052f9642673fb0a","secret":"0000000000000000000000000000000000000000000000000000000000000000","started_at":1564301772,"taker":"39c4bcdb1e6bbb29a3b131c2b82eba2552f4f8a804021b2064114ab857f00848","taker_amount":"0.999999999999999880468812552729","taker_coin":"KMD","taker_coin_start_block":1462209,"taker_payment_confirmations":1,"uuid":"8f5b267a-efa8-49d6-a92d-ec0523cca891"},"type":"Started"},"timestamp":1564301773193},{"event":{"data":{"taker_payment_locktime":1564309572,"taker_pubkey":"0339c4bcdb1e6bbb29a3b131c2b82eba2552f4f8a804021b2064114ab857f00848"},"type":"Negotiated"},"timestamp":1564301813664},{"event":{"data":{"block_height":0,"coin":"KMD","fee_details":{"amount":5.68e-05},"from":["RGPTERJVzcNK2n8xrW1yYHp9p715rLWxyn"],"internal_id":"cf54a5f5dfdf2eb404855eaba6a05b41f893a20327d43770c0138bb9ed2cf9eb","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":0,"to":["RGPTERJVzcNK2n8xrW1yYHp9p715rLWxyn","RThtXup6Zo7LZAi8kRWgjAyi1s4u6U9Cpf"],"total_amount":14.21857411,"tx_hash":"cf54a5f5dfdf2eb404855eaba6a05b41f893a20327d43770c0138bb9ed2cf9eb","tx_hex":"0400008085202f89018f03a4d46831ec541279d01998be6092a98ee0f103b69ab84697cdc3eea7e93c000000006a473044022046eb76ecf610832ef063a6d210b5d07bc90fd0f3b68550fd2945ce86b317252a02202d3438d2e83df49f1c8ab741553af65a0d97e6edccbb6c4d0c769b05426c637001210339c4bcdb1e6bbb29a3b131c2b82eba2552f4f8a804021b2064114ab857f00848ffffffff0276c40100000000001976a914ca1e04745e8ca0c60d8c5881531d51bec470743f88acddf7bd54000000001976a9144df806990ae0197402aeaa6d9b1ec60078d9eadf88ac01573d5d000000000000000000000000000000"},"type":"TakerFeeValidated"},"timestamp":1564301864738},{"event":{"data":{"block_height":0,"coin":"VRSC","fee_details":{"amount":1e-05},"from":["RXcUjam1KC8mA1hj33vXaX877jf7GgvKzt"],"internal_id":"2252c9929707995aff6dbb03d23b7e7eb786611d26b6ae748ca13007e71d1de6","my_balance_change":-9.38456187,"received_by_me":1243.91076118,"spent_by_me":1253.29532305,"timestamp":0,"to":["RXcUjam1KC8mA1hj33vXaX877jf7GgvKzt","bXAi6mfq2CzC4XvhVUgcTRhS1G5Y2pMf1R"],"total_amount":1253.29532305,"tx_hash":"2252c9929707995aff6dbb03d23b7e7eb786611d26b6ae748ca13007e71d1de6","tx_hex":"0400008085202f8901f63aed15c53b794df1a9446755f452e9fd9db250e1f608636f6172b7d795358c010000006b483045022100b5adb583fbb4b1a628b9c58ec292bb7b1319bb881c2cf018af6fe33b7a182854022020d89a2d6cbf15a117e2e1122046941f95466af7507883c4fa05955f0dfb81f2012103c2e08e48e6541b3265ccd430c5ecec7efc7d0d9fc4e310a9b052f9642673fb0affffffff0293b0ef370000000017a914ca41def369fc07d8aea10ba26cf3e64a12470d4087163149f61c0000001976a914f4f89313803d610fa472a5849d2389ca6df3b90088ac285a3d5d000000000000000000000000000000"},"type":"MakerPaymentSent"},"timestamp":1564301867675},{"event":{"data":{"error":"timeout (2690.6 > 2690.0)"},"type":"TakerPaymentValidateFailed"},"timestamp":1564304558269},{"event":{"data":{"block_height":0,"coin":"VRSC","fee_details":{"amount":1e-05},"from":["bXAi6mfq2CzC4XvhVUgcTRhS1G5Y2pMf1R"],"internal_id":"96d0b50bc2371ab88052bc4d656f1b91b3e3e64eba650eac28ebce9387d234cb","my_balance_change":9.38454187,"received_by_me":9.38454187,"spent_by_me":0,"timestamp":0,"to":["RXcUjam1KC8mA1hj33vXaX877jf7GgvKzt"],"total_amount":9.38455187,"tx_hash":"96d0b50bc2371ab88052bc4d656f1b91b3e3e64eba650eac28ebce9387d234cb","tx_hex":"0400008085202f8901e61d1de70730a18c74aeb6261d6186b77e7e3bd203bb6dff5a99079792c9522200000000b647304402207d36206295eee6c936d0204552cc5a001d4de4bbc0c5ae1c6218cf8548b4f08b02204c2a6470e06a6caf407ea8f2704fdc1b1dee39f89d145f8c0460130cb1875b2b01514c6b6304bc963d5db1752103c2e08e48e6541b3265ccd430c5ecec7efc7d0d9fc4e310a9b052f9642673fb0aac6782012088a9145f5598259da7c0c0beffcc3e9da35e553bac727388210339c4bcdb1e6bbb29a3b131c2b82eba2552f4f8a804021b2064114ab857f00848ac68feffffff01abacef37000000001976a914f4f89313803d610fa472a5849d2389ca6df3b90088ac26973d5d000000000000000000000000000000"},"type":"MakerPaymentRefunded"},"timestamp":1564321080407},{"event":{"type":"Finished"},"timestamp":1564321080409}],"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"uuid":"8f5b267a-efa8-49d6-a92d-ec0523cca891"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, maker_saved_swap));
        assert!(maker_swap.recover_funds().is_err());
    }

    #[test]
    fn swap_must_not_lock_funds_by_default() {
        let maker_saved_json = r#"{"error_events":["StartFailed","NegotiateFailed","TakerFeeValidateFailed","MakerPaymentTransactionFailed","MakerPaymentDataSendFailed","TakerPaymentValidateFailed","TakerPaymentSpendFailed","MakerPaymentRefunded","MakerPaymentRefundFailed"],"events":[{"event":{"data":{"lock_duration":7800,"maker_amount":"3.54932734","maker_coin":"KMD","maker_coin_start_block":1452970,"maker_payment_confirmations":1,"maker_payment_lock":1563759539,"my_persistent_pub":"031bb83b58ec130e28e0a6d5d2acf2eb01b0d3f1670e021d47d31db8a858219da8","secret":"0000000000000000000000000000000000000000000000000000000000000000","started_at":1563743939,"taker":"101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9","taker_amount":"0.02004833998671660000000000","taker_coin":"ETH","taker_coin_start_block":8196380,"taker_payment_confirmations":1,"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"},"type":"Started"},"timestamp":1563743939211},{"event":{"data":{"taker_payment_locktime":1563751737,"taker_pubkey":"03101ace6b08605b9424b0582b5cce044b70a3c8d8d10cb2965e039b0967ae92b9"},"type":"Negotiated"},"timestamp":1563743979835},{"event":{"data":{"block_height":8196386,"coin":"ETH","fee_details":null,"from":["0x3D6a2f4Dd6085b34EeD6cBc2D3aaABd0D3B697C1"],"internal_id":"00","my_balance_change":0,"received_by_me":0,"spent_by_me":0,"timestamp":1563744052,"to":["0xD8997941Dd1346e9231118D5685d866294f59e5b"],"total_amount":0.0001,"tx_hash":"a59203eb2328827de00bed699a29389792906e4f39fdea145eb40dc6b3821bd6","tx_hex":"f8690284ee6b280082520894d8997941dd1346e9231118d5685d866294f59e5b865af3107a4000801ca0743d2b7c9fad65805d882179062012261be328d7628ae12ee08eff8d7657d993a07eecbd051f49d35279416778faa4664962726d516ce65e18755c9b9406a9c2fd"},"type":"TakerFeeValidated"},"timestamp":1563744052878}],"success_events":["Started","Negotiated","TakerFeeValidated","MakerPaymentSent","TakerPaymentReceived","TakerPaymentWaitConfirmStarted","TakerPaymentValidatedAndConfirmed","TakerPaymentSpent","Finished"],"uuid":"3447b727-fe93-4357-8e5a-8cf2699b7e86"}"#;
        let maker_saved_swap: MakerSavedSwap = unwrap!(json::from_str(maker_saved_json));
        let key_pair = unwrap!(key_pair_from_seed("spice describe gravity federal blast come thank unfair canal monkey style afraid"));
        let ctx = MmCtxBuilder::default().with_secp256k1_key_pair(key_pair).into_mm_arc();

        TestCoin::ticker.mock_safe(|_| MockResult::Return("ticker"));
        let maker_coin = MmCoinEnum::Test(TestCoin {});
        let taker_coin = MmCoinEnum::Test(TestCoin {});
        let (_maker_swap, _) = unwrap!(MakerSwap::load_from_saved(ctx.clone(), maker_coin, taker_coin, maker_saved_swap));
        assert_eq!(get_locked_amount(&ctx, "ticker"), BigDecimal::from(0));
    }
}
