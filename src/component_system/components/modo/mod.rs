mod time;
use super::utils;
use super::utils::{
    app_command::{get_argument, ApplicationCommandEmbed},
    message, Data,
};
use crate::component_system::{self as cmp, command_parser as cmd};
use chrono::{DateTime, Utc};
use futures_locks::RwLock;
use serde::{Deserialize, Serialize};
use serenity::model::{
    event::ReadyEvent,
    id::{ApplicationId, GuildId},
    interactions::application_command::ApplicationCommandInteraction,
    prelude::*,
};
use serenity::{async_trait, client::Context};
use tokio::sync::oneshot::Sender;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum TypeModeration {
    Ban,
    Mute,
}

impl std::fmt::Display for TypeModeration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeModeration::Ban => write!(f, "ban"),
            TypeModeration::Mute => write!(f, "mute"),
        }
    }
}
impl TypeModeration {
    fn as_str(&self) -> &'static str {
        match self {
            TypeModeration::Ban => "ban",
            TypeModeration::Mute => "mute",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Action {
    type_mod: TypeModeration,
    user_id: u64,
    time: i64,
}

impl Action {
    fn new(type_mod: TypeModeration, user_id: u64, time: i64) -> Self {
        Self {
            type_mod,
            user_id,
            time,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct ModerationData {
    mod_until: Vec<Action>,
    muted_role: u64,
}
#[derive(Debug)]
pub struct Moderation {
    node: cmd::Node,
    owners: Vec<UserId>,
    app_id: ApplicationId,
    data: RwLock<Data<ModerationData>>,
    tasks: RwLock<Vec<(UserId, TypeModeration, Sender<()>)>>,
}

#[async_trait]
impl cmp::Component for Moderation {
    fn name(&self) -> &'static str {
        "mod"
    }

    async fn command(
        &self,
        _: &cmp::FrameworkConfig,
        _: &cmp::Context,
        _: &cmp::Message,
    ) -> cmp::CommandMatch {
        cmp::CommandMatch::NotMatched
    }

    async fn event(&self, ctx: &cmp::Context, evt: &cmp::Event) -> Result<(), String> {
        self.r_event(ctx, evt).await
    }
    fn node(&self) -> Option<&cmd::Node> {
        Some(&self.node)
    }
}

impl Moderation {
    pub fn new(app_id: ApplicationId, owners: Vec<UserId>) -> Moderation {
        let ban = cmd::Command::new("ban")
            .set_help(
                "Bannir un membre du serveur. Temporaire si le parametre *pendant* est renseigné.",
            )
            .add_param(
                cmd::Argument::new("qui")
                    .set_value_type(cmd::ValueType::User)
                    .set_help("Le membre à bannir")
                    .set_required(true),
            )
            .add_param(
                cmd::Argument::new("pourquoi")
                    .set_value_type(cmd::ValueType::String)
                    .set_help("La raison du ban")
                    .set_required(true),
            )
            .add_param(
                cmd::Argument::new("pendant")
                    .set_value_type(cmd::ValueType::String)
                    .set_help("Pendant combien de temps"),
            );
        let mute = ban.clone()
            .set_name("mute")
            .set_help("Attribue le rôle *muted* à un membre. Temporaire si le parametre *pendant* est renseigné.");
        let unban = cmd::Command::new("unban")
            .set_help("Unban un membre")
            .add_param(
                cmd::Argument::new("qui")
                    .set_value_type(cmd::ValueType::User)
                    .set_help("Le membre à unban")
                    .set_required(true),
            );
        let unmute = unban
            .clone()
            .set_name("unmute")
            .set_help("Retire le rôle *muted* à un membre.");
        let node = cmd::Node::new()
            .add_command(ban)
            .add_command(mute)
            .add_command(unban)
            .add_command(unmute);
        Moderation {
            node,
            app_id,
            data: match Data::from_file_default("moderation") {
                Ok(data) => RwLock::new(data),
                Err(e) => panic!("Data moderation: {:?}", e),
            },
            owners,
            tasks: RwLock::new(Vec::new()),
        }
    }
    // region: discord interface
    async fn r_event(&self, ctx: &cmp::Context, evt: &cmp::Event) -> Result<(), String> {
        use cmp::Event::*;
        use serenity::model::interactions::Interaction::*;
        match evt {
            Ready(ReadyEvent { ready, .. }) => self.on_ready(ctx, ready).await,
            InteractionCreate(InteractionCreateEvent {
                interaction: ApplicationCommand(c),
                ..
            }) => self.on_applications_command(ctx, c).await,
            _ => Ok(()),
        }
    }
    async fn on_ready(
        &self,
        ctx: &cmp::Context,
        ready: &serenity::model::gateway::Ready,
    ) -> Result<(), String> {
        let (mod_until, muted_role, guild_id) = {
            let data = self.data.read().await;
            let data = data.read();

            let guild_id = ready
                .guilds
                .iter()
                .map(|g| g.id())
                .next()
                .ok_or_else(|| "No guild found".to_string())?;
            (data.mod_until.clone(), data.muted_role, guild_id)
        };
        if muted_role == 0 {
            let role = guild_id
                .roles(ctx)
                .await
                .map_err(|e| format!("Impossible d'obtenir la liste des roles du serveur: {}", e))?
                .into_iter()
                .find(|(_, role)| role.name == "muted")
                .ok_or_else(|| "Impossible de trouver le role muted".to_string())?;
            self.data.write().await.write().muted_role = role.0 .0;
        }
        futures::future::join_all(
            mod_until
                .into_iter()
                .map(|act| self.make_task(ctx.clone(), guild_id, act)),
        )
        .await;
        Ok(())
    }
    async fn on_applications_command(
        &self,
        ctx: &Context,
        app_command: &ApplicationCommandInteraction,
    ) -> Result<(), String> {
        if app_command.application_id != self.app_id {
            // La commande n'est pas destiné à ce bot
            return Ok(());
        }
        let app_cmd = ApplicationCommandEmbed::new(app_command);
        let guild_id = match app_cmd.get_guild_id() {
            Some(v) => v,
            None => {
                return Err("Vous devez être dans un serveur pour utiliser cette commande.".into())
            }
        };
        let command_name = app_cmd.fullname();
        let msg = match command_name.as_str() {
            "ban" => {
                self.moderate(ctx, guild_id, &app_cmd, TypeModeration::Ban, false)
                    .await
            }
            "mute" => {
                self.moderate(ctx, guild_id, &app_cmd, TypeModeration::Mute, false)
                    .await
            }
            "unban" => {
                self.moderate(ctx, guild_id, &app_cmd, TypeModeration::Ban, true)
                    .await
            }
            "unmute" => {
                self.moderate(ctx, guild_id, &app_cmd, TypeModeration::Mute, true)
                    .await
            }
            _ => return Ok(()),
        }
        .or_else(|e| -> Result<message::Message, ()> { Ok(message::error(e).set_ephemeral(true)) })
        .unwrap();

        app_command
            .create_interaction_response(ctx, |resp| {
                *resp = msg.into();
                resp
            })
            .await
            .map_err(|e| format!("Cannot create response: {}", e))
    }
    // endregion: discord interface
    // region: tasks
    async fn task(
        ctx: Context,
        guild_id: GuildId,
        action: Action,
        data: RwLock<Data<ModerationData>>,
    ) {
        let time_point =
            DateTime::<Utc>::from_utc(chrono::NaiveDateTime::from_timestamp(action.time, 0), Utc);
        let duration = time_point - chrono::Utc::now();
        if duration.num_seconds() > 0 {
            tokio::time::sleep(duration.to_std().unwrap()).await;
        }
        let action_done = match action.type_mod {
            TypeModeration::Mute => {
                let mut member = match guild_id.member(&ctx, action.user_id).await {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("Impossible d'avoir le membre {}: {}", action.user_id, e);
                        return;
                    }
                };

                let muted_role = { data.read().await.read().muted_role };
                member.remove_role(&ctx, muted_role).await
            }
            TypeModeration::Ban => guild_id.unban(&ctx, action.user_id).await,
        };
        let username = UserId(action.user_id)
            .to_user(&ctx)
            .await
            .map(|user| format!("{}#{} ({})", user.name, user.discriminator, action.user_id))
            .unwrap_or_else(|_| action.user_id.to_string());
        if let Err(e) = action_done {
            eprintln!("modo::task erreur {}: {}", username, e);
        } else {
            println!("modo::task: Sanction contre {} retiré", username);
            let mut data = data.write().await;
            let mut data = data.write();
            let mod_until = &mut data.mod_until;

            match mod_until
                .iter()
                .position(|Action { user_id, .. }| user_id == &action.user_id)
                .map(|idx| mod_until.remove(idx))
            {
                Some(_) => (),
                None => eprintln!(
                    "modo::task: sanction non trouvée dans les données pour l'utilisateur {}",
                    username
                ),
            };
        }
    }
    async fn make_task(&self, ctx: Context, guild_id: GuildId, action: Action) {
        let who = match guild_id.member(&ctx, action.user_id).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Impossible d'avoir le membre {}: {}", action.user_id, e);
                return;
            }
        };
        let task = Self::task(ctx, guild_id, action.clone(), self.data.clone());
        let (stop_task, stop_me) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tokio::select! {
                _ = task => println!("{} du membre {} fini", action.type_mod, who.display_name()),
                _ = stop_me => println!("Arrêt {} temporaire de {}", action.type_mod, who.display_name()),
            }
        });
        self.tasks
            .write()
            .await
            .push((UserId(action.user_id), action.type_mod, stop_task));
    }
    async fn remove_task(&self, who: UserId, type_mod: TypeModeration) {
        let mut tasks = self.tasks.write().await;
        let idx = match tasks
            .iter()
            .position(|(user_id, t, _)| user_id == &who && t == &type_mod)
        {
            Some(idx) => idx,
            None => return,
        };
        let (_, _, stop_task) = tasks.remove(idx);
        stop_task.send(()).unwrap_or(());
    }
    async fn add_until(&self, who: u64, when: i64, what: TypeModeration) -> Action {
        let mut data = self.data.write().await;
        let mut data = data.write();
        let result = Action::new(what, who, when);
        data.mod_until.push(result.clone());
        result
    }
    async fn remove_until(&self, who: u64, what: TypeModeration) {
        let mut data = self.data.write().await;
        let mut data = data.write();
        data.mod_until
            .iter()
            .position(|a| a.user_id == who && a.type_mod == what)
            .map(|idx| {
                data.mod_until.remove(idx);
            })
            .unwrap_or_default();
    }
    // endregion
    // region: actions
    async fn moderate(
        &self,
        ctx: &Context,
        guild_id: GuildId,
        app_cmd: &ApplicationCommandEmbed<'_>,
        what: TypeModeration,
        disable: bool,
    ) -> Result<message::Message, String> {
        let user_cmd = &app_cmd.0.member.as_ref().unwrap().user;
        let what_str = format!("{}{}", if disable { "un" } else { "" }, what.as_str());
        let user = get_argument!(app_cmd, "qui", User)
            .map(|v| v.0)
            .ok_or_else(|| "Vous devez mentionner un membre.".to_string())
            .and_then(|user| {
                if user.id != user_cmd.id {
                    Ok(user)
                } else {
                    Err(format!("Vous ne pouvez pas vous {} vous-même.", &what_str))
                }
            })?;
        let reason = if !disable {
            Some(
                get_argument!(app_cmd, "pourquoi", String)
                    .map(|v| v)
                    .ok_or_else(|| "Raison non specifiée.".to_string())?,
            )
        } else {
            None
        };
        let time = match (disable, get_argument!(app_cmd, "pendant", String)) {
            (false, Some(v)) => {
                let duration_second = match time::parse(v) {
                    Ok(v) => v as _,
                    Err(e) => return Ok(message::error(e).set_ephemeral(true)),
                };
                let duration = chrono::Duration::seconds(duration_second);
                let time_point = chrono::Local::now() + duration;
                Some((time_point.timestamp(), time_point, v))
            }
            _ => None,
        };
        let muted_role = if what == TypeModeration::Mute {
            let muted_role = self.data.read().await.read().muted_role;
            if muted_role == 0 {
                return Err("Le rôle de mute n'est pas défini.".into());
            }
            Some(RoleId(muted_role))
        } else {
            None
        };
        if !disable {
            let when = time.map(|(_, when, _)| when.format("%d/%m/%Y à %H:%M:%S").to_string());
            match self
                .warn_member(
                    ctx,
                    user,
                    &what_str,
                    when.as_deref(),
                    reason.map(|v| v.as_str()).unwrap(),
                    guild_id.name(ctx).await.unwrap().as_str(),
                )
                .await
            {
                Err(e) => println!("[WARN] Impossible d'avertir le membre: {}", e),
                _ => (),
            }
        }
        Self::do_action(ctx, guild_id, user.id, what, disable, reason, muted_role)
            .await
            .map_err(|e| format!("Impossible de {} le membre: {}", what_str, e))?;

        tokio::join!(
            self.remove_task(user.id, what),
            self.remove_until(user.id.0, what)
        );

        let username = format!("{}#{} (<@{}>)", user.name, user.discriminator, user.id);
        let who_did = format!("{}#{}", user_cmd.name, user_cmd.discriminator);

        Self::write_log(
            &username,
            &who_did,
            &what_str,
            reason.map(|v| v.as_str()),
            time.map(|v| v.2.as_str()),
        )
        .await;

        let mut msg = message::success(format!("{} a été {}.", username, what_str));
        if let Some(reason) = reason {
            msg.embed.as_mut().unwrap().field("Raison", reason, false);
        }
        if let Some((timestamp, datetime, duration)) = time {
            self.make_task(
                ctx.clone(),
                guild_id,
                self.add_until(user.id.0, timestamp, what).await,
            )
            .await;
            msg.embed
                .as_mut()
                .unwrap()
                .field("Pendant", duration, false);
            msg.embed.as_mut().unwrap().field(
                "Prend fin",
                datetime.format("%d/%m/%Y à %H:%M:%S").to_string(),
                true,
            );
        }
        Ok(msg)
    }
    async fn warn_member(
        &self,
        ctx: &Context,
        user: &User,
        keyword: &str,
        when: Option<&str>,
        reason: &str,
        guild_name: &str,
    ) -> Result<(), String> {
        match user.direct_message(ctx, |msg| {
            if let Some(when) = when {
                msg.content(format!("Vous avez été temporairement **{}** du serveur {}.\n__Raison__ : {}\n__Prend fin le__ : {}", keyword, guild_name, reason, when));
            } else {
                msg.content(format!("Vous avez été **{}** du serveur {}.\n__Raison__ : {}", keyword, guild_name, reason));
            }
            msg
        }).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let username = format!("{}#{}", user.name, user.discriminator);
                Err(format!("Impossible d'envoyer le message de bannissement à l'utilisateur {}: {}", username, e))
            }
        }
    }
    async fn do_action(
        ctx: &Context,
        guild_id: GuildId,
        user: UserId,
        what: TypeModeration,
        disable: bool,
        reason: Option<&String>,
        muted_role: Option<RoleId>,
    ) -> serenity::Result<()> {
        match (what, disable, reason) {
            (TypeModeration::Ban, false, Some(reason)) => {
                guild_id.ban_with_reason(&ctx, user, 0, reason).await?
            }
            (TypeModeration::Ban, false, None) => guild_id.ban(&ctx, user, 0).await?,
            (TypeModeration::Mute, false, _) => {
                if let Some(muted_role) = muted_role {
                    let mut member = guild_id.member(ctx, user).await?;
                    member.add_role(ctx, muted_role).await?;
                }
            }
            (TypeModeration::Ban, true, _) => guild_id.unban(&ctx, user).await?,
            (TypeModeration::Mute, true, _) => {
                if let Some(muted_role) = muted_role {
                    let mut member = guild_id.member(ctx, user).await?;
                    member.remove_role(ctx, muted_role).await?;
                }
            }
        };
        Ok(())
    }
    // endregion
    async fn write_log(
        who: &str,
        who_did: &str,
        what: &str,
        why: Option<&str>,
        during: Option<&str>,
    ) {
        use std::io::Write;
        use tokio::fs::OpenOptions;
        let file_path = utils::DATA_DIR.join("modo.log");
        let file = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                println!("Impossible d'ouvrir le fichier de log: {}", e);
                return;
            }
        };
        let now = chrono::Local::now();
        let mut file: std::fs::File = file.into_std().await;
        let file = &mut file;
        match (|| -> std::io::Result<()> {
            write!(
                file,
                "{:=<10}\nWhen: {}\nWho: {}\nWhat: {}\nWho did: {}\n",
                "",
                now.to_rfc3339(),
                who,
                what,
                who_did
            )?;
            if let Some(why) = why {
                writeln!(file, "Why: {}", why)?;
            }
            if let Some(during) = during {
                writeln!(file, "During: {}", during)?;
            }
            Ok(())
        })() {
            Ok(_) => (),
            Err(e) => {
                println!("Impossible d'écrire dans le fichier de log: {}", e);
            }
        }
    }
}
