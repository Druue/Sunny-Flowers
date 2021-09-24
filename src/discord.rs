use std::{
    collections::HashSet,
    env,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use dotenv::dotenv;

use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    framework::{
        standard::{
            help_commands,
            macros::{command, group, help},
            Args, CommandGroup, CommandResult, HelpOptions,
        },
        StandardFramework,
    },
    http::Http,
    model::{
        channel::Message,
        gateway::Ready,
        guild::Guild,
        misc::Mentionable,
        prelude::{ChannelId, UserId},
    },
    prelude::Mutex,
    Result as SerenityResult,
};
use songbird::input::restartable::Restartable;
use songbird::{Event, EventContext, EventHandler as VoiceEventHandler, SerenityInit, TrackEvent};

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: Ready) {
        println!("{} is connected", ready.user.name);
    }
}

struct TrackEndNotifier {
    channel_id: ChannelId,
    http: Arc<Http>,
}

#[async_trait]
impl VoiceEventHandler for TrackEndNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(track_list) = ctx {
            check_msg(
                self.channel_id
                    .say(&self.http, format!("Tracks ended: {}", track_list.len()))
                    .await,
            );
        }

        None
    }
}

struct TimeoutHandler {
    guild: Guild,
    channel_id: ChannelId,
    timer: AtomicUsize,
    ctx: Context,
}

#[async_trait]
impl VoiceEventHandler for TimeoutHandler {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        if check_alone(
            &self.guild,
            self.channel_id,
            self.ctx.cache.current_user_id().await,
        ) {
            let prev = self.timer.fetch_add(1, Ordering::Relaxed);

            if prev >= 5 {
                let manager = songbird::get(&self.ctx)
                    .await
                    .expect("Songbird Voice Client placed in at initialisation")
                    .clone();

                if let Err(e) = manager.remove(self.guild.id).await {
                    eprintln!("Failed: {:?}", e);
                }

                check_msg(
                    self.channel_id
                        .say(&self.ctx.http, "Left voice due to lack of frens :(((")
                        .await,
                );
            }
        } else {
            let _ = self.timer.swap(0, Ordering::Relaxed);
        }

        None
    }
}

fn check_alone(guild: &Guild, channel_id: ChannelId, bot_id: UserId) -> bool {
    !guild.voice_states.values().any(|vs| match vs.channel_id {
        Some(c_id) => channel_id.0 == c_id.0 && vs.user_id.0 != bot_id.0,
        None => false,
    })
}

#[group]
#[commands(join, leave, play, ping, skip, stop)]
struct General;

#[help]
async fn help(
    ctx: &Context,
    msg: &Message,
    args: Args,
    help_options: &'static HelpOptions,
    groups: &[&'static CommandGroup],
    owners: HashSet<UserId>,
) -> CommandResult {
    let _ = help_commands::with_embeds(ctx, msg, args, help_options, groups, owners).await;
    Ok(())
}

#[command]
#[only_in(guilds)]
/// Adds Sunny to the user's current voice channel.
async fn join(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    let channel_id = guild
        .voice_states
        .get(&msg.author.id)
        .and_then(|voice_state| voice_state.channel_id);

    let connect_to = match channel_id {
        Some(channel) => channel,
        None => {
            check_msg(msg.reply(ctx, "Not in a voice").await);

            return Ok(());
        }
    };

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice Client placed in at initialisation")
        .clone();

    let (handle_lock, success) = manager.join(guild_id, connect_to).await;

    if let Ok(_channel) = success {
        check_msg(
            msg.channel_id
                .say(&ctx.http, format!("Joined {}", connect_to.mention()))
                .await,
        );

        let channel_id = msg.channel_id;
        let send_http = ctx.http.clone();
        let mut handle = handle_lock.lock().await;

        handle.add_global_event(
            Event::Track(TrackEvent::End),
            TrackEndNotifier {
                channel_id,
                http: send_http,
            },
        );

        handle.add_global_event(
            Event::Periodic(Duration::from_secs(60), None),
            TimeoutHandler {
                guild,
                channel_id,
                timer: Default::default(),
                ctx: ctx.clone(),
            },
        )
    } else {
        check_msg(
            msg.channel_id
                .say(&ctx.http, "Failed to join channel")
                .await,
        );
    }

    deafen(handle_lock).await;

    Ok(())
}

async fn deafen(handler_lock: Arc<Mutex<songbird::Call>>) {
    let mut handler = handler_lock.lock().await;

    if handler.is_deaf() {
        println!("Client already deafened");
    } else if let Err(e) = handler.deafen(true).await {
        eprintln!("Failed: {:?}", e)
    }
}

#[command]
#[only_in(guilds)]
/// Removes Sunny from the current voice channel and clears the queue.
async fn leave(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice Client placed in at initialisation")
        .clone();
    let has_handler = manager.get(guild_id).is_some();

    if has_handler {
        if let Err(e) = manager.remove(guild_id).await {
            check_msg(
                msg.channel_id
                    .say(&ctx.http, format!("Failed: {:?}", e))
                    .await,
            );
        }

        check_msg(msg.channel_id.say(&ctx.http, "Left voice").await);
    } else {
        check_msg(msg.reply(ctx, "Not in a voice channel").await);
    }

    Ok(())
}

#[command]
#[aliases(p)]
#[max_args(1)]
#[only_in(guilds)]
#[usage("<url>")]
#[example("https://www.youtube.com/watch?v=dQw4w9WgXcQ")]
/// While Sunny is in a voice channel, you may run the play command so that she
/// can start streaming the given video URL.
async fn play(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
    let url = match args.single::<String>() {
        Ok(url) => url,
        Err(_) => {
            check_msg(
                msg.channel_id
                    .say(&ctx.http, "Must provide a URL to a video or audio")
                    .await,
            );

            return Ok(());
        }
    };

    if !url.starts_with("http") {
        check_msg(
            msg.channel_id
                .say(&ctx.http, "Must provide a valid URL")
                .await,
        );

        return Ok(());
    }

    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice Client placed in at initialisation")
        .clone();

    if let Some(handler_lock) = manager.get(guild_id) {
        let mut handler = handler_lock.lock().await;

        let source = match Restartable::ytdl(url, true).await {
            Ok(source) => source,
            Err(why) => {
                println!("Err starting source {:?}", why);
                check_msg(msg.channel_id.say(&ctx.http, "Error sourcing ffmpeg").await);

                return Ok(());
            }
        };

        handler.enqueue_source(source.into());
        check_msg(
            msg.channel_id
                .say(
                    &ctx.http,
                    format!("Added song to queue: position {}", handler.queue().len()),
                )
                .await,
        );
    } else {
        check_msg(
            msg.channel_id
                .say(&ctx.http, "Not in a voice channel to play in")
                .await,
        );
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
/// Skips the currently playing song and moves to the next song in the queue.
async fn skip(ctx: &Context, msg: &Message, _args: Args) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice Client placed in at initialisation")
        .clone();

    if let Some(handler_lock) = manager.get(guild_id) {
        let handler = handler_lock.lock().await;
        let queue = handler.queue();
        let _ = queue.skip();

        check_msg(
            msg.channel_id
                .say(
                    &ctx.http,
                    format!("Song skipped: {} in queue.", queue.len()),
                )
                .await,
        );
    } else {
        check_msg(
            msg.channel_id
                .say(&ctx.http, "Not in a voice channel")
                .await,
        );
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
/// Stops playing the current song and clears the current song queue.
async fn stop(ctx: &Context, msg: &Message, _args: Args) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice Client placed in at initialisation")
        .clone();

    if let Some(handler_lock) = manager.get(guild_id) {
        let handler = handler_lock.lock().await;
        let queue = handler.queue();
        let _ = queue.stop();

        check_msg(msg.channel_id.say(&ctx.http, "Queue cleared.").await);
    } else {
        check_msg(
            msg.channel_id
                .say(&ctx.http, "Not in a voice channel")
                .await,
        );
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
/// Pong
async fn ping(ctx: &Context, msg: &Message) -> CommandResult {
    check_msg(msg.channel_id.say(&ctx.http, "Pong!").await);

    Ok(())
}

fn check_msg(result: SerenityResult<Message>) {
    if let Err(why) = result {
        println!("Error sending message: {:?}", why);
    }
}

pub async fn create_bot() {
    let _ = dotenv();
    let token = env::var("DISCORD_TOKEN").expect("Environment variable DISCORD_TOKEN not found");

    let framework = StandardFramework::new()
        .configure(|c| c.prefix("!"))
        .group(&GENERAL_GROUP)
        .help(&HELP);

    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .framework(framework)
        .register_songbird()
        .await
        .expect("Error creating client");

    tokio::spawn(async move {
        let _ = client
            .start()
            .await
            .map_err(|why| println!("Client ended: {:?}", why));
    });
}
