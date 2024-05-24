use std::sync::atomic::Ordering;

use globed_shared::{crypto_box::ChaChaBox, logger::*, PROTOCOL_VERSION};

use crate::server_thread::{GameServerThread, PacketHandlingError};

use super::*;
use crate::data::*;

impl GameServerThread {
    gs_handler!(self, handle_ping, PingPacket, packet, {
        self.send_packet_static(&PingResponsePacket {
            id: packet.id,
            player_count: self.game_server.state.player_count.load(Ordering::Relaxed),
        })
        .await
    });

    gs_handler!(self, handle_crypto_handshake, CryptoHandshakeStartPacket, packet, {
        if packet.protocol != PROTOCOL_VERSION && packet.protocol != 0xffff {
            self.terminate();
            self.send_packet_static(&ProtocolMismatchPacket { protocol: PROTOCOL_VERSION }).await?;
            return Ok(());
        }

        {
            // as ServerThread is now tied to the SocketAddrV4 and not account id like in globed v0
            // erroring here is not a concern, even if the user's game crashes without a disconnect packet,
            // they would have a new randomized port when they restart and this would never fail.
            if self.crypto_box.get().is_some() {
                self.disconnect("attempting to perform a second handshake in one session").await?;
                return Err(PacketHandlingError::WrongCryptoBoxState);
            }

            self.crypto_box
                .get_or_init(|| ChaChaBox::new(&packet.key.0, &self.game_server.secret_key));
        }

        self.send_packet_static(&CryptoHandshakeResponsePacket {
            key: self.game_server.public_key.clone().into(),
        })
        .await
    });

    gs_handler!(self, handle_keepalive, KeepalivePacket, _packet, {
        let _ = gs_needauth!(self);

        self.send_packet_static(&KeepaliveResponsePacket {
            player_count: self.game_server.state.player_count.load(Ordering::Relaxed),
        })
        .await
    });

    gs_handler!(self, handle_login, LoginPacket, packet, {
        // if we have already logged in, ignore this login attempt
        if self.authenticated() {
            return Ok(());
        }

        // disconnect if server is under maintenance
        if self.game_server.bridge.central_conf.lock().maintenance {
            gs_disconnect!(self, "The server is currently under maintenance, please try connecting again later.");
        }

        if packet.fragmentation_limit < 1300 {
            gs_disconnect!(
                self,
                &format!(
                    "The client fragmentation limit is too low ({} bytes) to be accepted",
                    packet.fragmentation_limit
                )
            );
        }

        self.fragmentation_limit.store(packet.fragmentation_limit, Ordering::Relaxed);

        if packet.account_id <= 0 || packet.user_id <= 0 {
            self.terminate();
            let message = format!(
                "Invalid account/user ID was sent ({} and {}). Please note that you must be signed into a Geometry Dash account before connecting.",
                packet.account_id, packet.user_id
            );
            self.send_packet_dynamic(&LoginFailedPacket { message: &message }).await?;
            return Ok(());
        }

        // skip authentication if standalone
        let standalone = self.game_server.standalone;
        let player_name = if standalone {
            packet.name
        } else {
            // lets verify the given token
            let result = {
                self.game_server
                    .bridge
                    .token_issuer
                    .lock()
                    .validate(packet.account_id, packet.user_id, packet.token.to_str().unwrap())
            };

            match result {
                Ok(x) => InlineString::new(&x),
                Err(err) => {
                    self.terminate();

                    let mut message = FastString::new("authentication failed: ");
                    message.extend(err.error_message());

                    self.send_packet_dynamic(&LoginFailedPacket { message: &message }).await?;
                    return Ok(());
                }
            }
        };

        // check if the user is already logged in, kick the other instance
        self.game_server.check_already_logged_in(packet.account_id).await?;

        // fetch data from the central
        if !standalone {
            let user_entry = match self.game_server.bridge.get_user_data(&packet.account_id.to_string()).await {
                Ok(user) if user.is_banned => {
                    self.terminate();
                    self.send_packet_dynamic(&ServerBannedPacket {
                        message: (FastString::new(&format!(
                            "{}",
                            user.violation_reason.as_ref().map_or_else(|| "No reason given".to_owned(), |x| x.clone()),
                        ))),
                        timestamp: (user.violation_expiry.unwrap()),
                    })
                    .await?;

                    return Ok(());
                }
                Ok(user) if self.game_server.bridge.is_whitelist() && !user.is_whitelisted => {
                    self.terminate();
                    self.send_packet_dynamic(&LoginFailedPacket {
                        message: "This server has whitelist enabled and your account has not been allowed.",
                    })
                    .await?;

                    return Ok(());
                }
                Ok(user) => user,
                Err(err) => {
                    self.terminate();

                    let mut message = InlineString::<256>::new("failed to fetch user data: ");
                    message.extend_safe(&err.to_string());

                    self.send_packet_dynamic(&LoginFailedPacket { message: &message }).await?;
                    return Ok(());
                }
            };

            *self.user_role.lock() = self.game_server.state.role_manager.compute(&user_entry.user_roles);
            *self.user_entry.lock() = user_entry;
        }

        self.account_id.store(packet.account_id, Ordering::Relaxed);
        self.claim_secret_key.store(packet.secret_key, Ordering::Relaxed);
        self.game_server.state.player_count.fetch_add(1u32, Ordering::Relaxed); // increment player count

        info!(
            "Login successful from {player_name} (account ID: {}, address: {})",
            packet.account_id, self.tcp_peer
        );

        let special_user_data = {
            let mut account_data = self.account_data.lock();
            account_data.account_id = packet.account_id;
            account_data.user_id = packet.user_id;
            account_data.icons.clone_from(&packet.icons);
            account_data.name = player_name;

            let user_entry = self.user_entry.lock();
            let sud = SpecialUserData::from_user_entry(&*user_entry, &self.game_server.state.role_manager);

            account_data.special_user_data.clone_from(&sud);

            sud
        };

        // add them to the global room
        self.game_server.state.room_manager.get_global().manager.create_player(packet.account_id);

        let tps = self.game_server.bridge.central_conf.lock().tps;

        let all_roles = self.game_server.state.role_manager.get_all_roles();

        self.send_packet_dynamic(&LoggedInPacket {
            tps,
            special_user_data,
            all_roles,
        })
        .await?;

        Ok(())
    });

    gs_handler_sync!(self, handle_disconnect, DisconnectPacket, _packet, {
        self.terminate();
        Ok(())
    });

    gs_handler!(self, handle_keepalive_tcp, KeepaliveTCPPacket, _packet, {
        let _ = gs_needauth!(self);

        self.send_packet_static(&KeepaliveTCPResponsePacket).await
    });

    gs_handler!(self, handle_connection_test, ConnectionTestPacket, packet, {
        self.send_packet_dynamic(&ConnectionTestResponsePacket {
            uid: packet.uid,
            data: packet.data,
        })
        .await
    });
}
