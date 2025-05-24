import asyncio
import re
import yt_dlp
import traceback
import discord 
from spotipy import Spotify
from spotipy.oauth2 import SpotifyClientCredentials
from discord.ext import commands
from dotenv import load_dotenv
from collections import deque

def run_bot():

    sp = Spotify(auth_manager=SpotifyClientCredentials())



    load_dotenv()

    queues = {}
    is_loop = {}
    current_songs = {}
    disconnect_timers = {}

    with open('token.txt', 'r') as f:
        TOKEN = f.read().strip()

    client = discord.Client(intents=discord.Intents.all())

    voice_clients = {}
    
    yt_dl_options = {
        'format': 'bestaudio/best',
        'noplaylist': False,
        'nocheckcertificate': True,
        'ignoreerrors': False,
        'logtostderr': False,
        'quiet': True,
        'no_warnings': True,
        'default_search': 'auto',
        'cookiefile': 'cookies.txt',
        'postprocessors': [{
            'key': 'FFmpegExtractAudio',
            'preferredcodec': 'mp3',
            'preferredquality': '192',
        }],
    }

    ytdl = yt_dlp.YoutubeDL(yt_dl_options)

    ffmpeg_options = {
        'options': '-vn',
        'before_options': '-reconnect 1 -reconnect_streamed 1 -reconnect_delay_max 5'
    }

    def start_disconnect_timer(guild_id):
        if guild_id in disconnect_timers:
            disconnect_timers[guild_id].cancel()

        async def disconnect():
            await asyncio.sleep(120) # minutes
            if guild_id in voice_clients and not voice_clients[guild_id].is_playing():
                await voice_clients[guild_id].disconnect()
                voice_clients.pop(guild_id, None)
                queues.pop(guild_id, None)
                current_songs.pop(guild_id, None)
                is_loop.pop(guild_id, None)
                print(f'Auto-disconnected from {guild_id} due to inactivity.')

            task = asyncio.create_task(disconnect())
            disconnect_timers[guild_id] = task

    async def play_next_song(guild_id):
        if guild_id not in voice_clients:
            return

        if guild_id in queues and queues[guild_id]:
            next_song = queues[guild_id].popleft()
            current_songs[guild_id] = next_song
            
            voice_clients[guild_id].play(
                discord.FFmpegPCMAudio(next_song['url'], **ffmpeg_options),
                after=lambda e: asyncio.run_coroutine_threadsafe(
                    play_next_song(guild_id),
                    client.loop
                ).result()
            )
        elif guild_id in is_loop and is_loop[guild_id] and guild_id in current_songs:
            # if loop is enabled and we have a current song, replay it
            song = current_songs[guild_id]
            voice_clients[guild_id].play(
                discord.FFmpegPCMAudio(song['url'], **ffmpeg_options),
                after=lambda e: asyncio.run_coroutine_threadsafe(
                    play_next_song(guild_id),
                    client.loop
                ).result()
            )
        else: 
            start_disconnect_timer(guild_id)

    @client.event
    async def on_ready():
        print(f'{client.user} is now jamming.')

    @client.event
    async def on_message(message):
        if message.author.bot:
            return

        guild_id = message.guild.id

        if message.content.startswith('!!play'):
            try:
                if message.author.voice is None or message.author.voice.channel is None:
                    await message.channel.send('You need to be in a voice channel!')
                    return
                
                if guild_id not in queues:
                    queues[guild_id] = deque()
                    is_loop[guild_id] = False

                if guild_id not in voice_clients:
                    voice_client = await message.author.voice.channel.connect()
                    voice_clients[guild_id] = voice_client

                query = message.content[len('!!play '):].strip()
                
                try:
                    if 'open.spotify.com/track' in query:
                        match = re.search(r'https?://open\.spotify\.com/track/([a-zA-Z0-9]+)', query)
                        if match:
                            track_id = match.group(1)
                            track = sp.track(track_id)
                            track_title = track['name']
                            track_artist = track['artists'][0]['name']
                            query = f"{track_title} {track_artist}"
                            await message.channel.send(f"🎧 Found Spotify track: `{track_title}` by `{track_artist}`. Searching on YouTube...")
                    else:
                        await message.channel.send(f'🔍 Searching for `{query}`...')
                        
                    loop = asyncio.get_event_loop()
                    data = await loop.run_in_executor(
                        None, 
                        lambda: ytdl.extract_info(f"ytsearch:{query}", download=False)
                    )
                    
                    if 'entries' not in data or not data['entries']:
                        await message.channel.send("❌ No results found.")
                        return

                    video = data['entries'][0]
                    if not video:
                        await message.channel.send("❌ Could not find video.")
                        return

                    song_info = {
                        'url': video['url'],
                        'title': video['title']
                    }

                    # add to queue
                    queues[guild_id].append(song_info)
                    position = len(queues[guild_id])

                    # if nothing is playing, start playing
                    if not voice_clients[guild_id].is_playing():
                        await play_next_song(guild_id)
                        await message.channel.send(f"🎶 Now playing: `{song_info['title']}`")
                    else:
                        await message.channel.send(f"📝 Added to queue (#{position}): `{song_info['title']}`")

                except Exception as e:
                    print(f"Error during playback setup: {str(e)}")
                    traceback.print_exc()
                    await message.channel.send(f"⚠️ Error: {str(e)}")

            except Exception as e:
                print(f"Outer error: {str(e)}")
                traceback.print_exc()
                await message.channel.send(f"⚠️ An error occurred: {str(e)}")

        elif message.content.startswith('!!queue'):
            if guild_id in queues and queues[guild_id]:
                queue_list = "\n".join([
                    f"{i+1}. {song['title']}"
                    for i, song in enumerate(queues[guild_id])
                ])
                current = current_songs.get(guild_id, {}).get('title', 'Nothing')
                await message.channel.send(f"**Now Playing:** `{current}`\n\n**Queue:**\n{queue_list}")
            else:
                await message.channel.send("Queue is empty!")

        elif message.content.startswith('!!skip'):
            if guild_id in voice_clients and voice_clients[guild_id].is_playing():
                voice_clients[guild_id].stop()  # This will trigger play_next_song
                await message.channel.send("⏭️ Skipped to next song.")
            else:
                await message.channel.send("❌ Nothing to skip.")

        elif message.content.startswith('!!clear'):
            if guild_id in queues:
                queues[guild_id].clear()
                await message.channel.send("🗑️ Queue cleared.")
            else:
                await message.channel.send("❌ No queue to clear.")

        elif message.content.startswith('!!pause'):
            try:
                if guild_id in voice_clients and voice_clients[guild_id].is_playing():
                    voice_clients[guild_id].pause()
                    await message.channel.send("⏸️ Music paused.")
                else:
                    await message.channel.send("❌ No music is playing.")
            except Exception as e:
                print(f"Error during pause: {str(e)}")
                traceback.print_exc()

        elif message.content.startswith('!!resume'):
            try:
                if guild_id in voice_clients and voice_clients[guild_id].is_paused():
                    voice_clients[guild_id].resume()
                    await message.channel.send("▶️ Music resumed.")
                else:
                    await message.channel.send("❌ No music is paused.")
            except Exception as e:
                print(f"Error during resume: {str(e)}")
                traceback.print_exc()

        elif message.content.startswith('!!loop'):
            is_loop[guild_id] = not is_loop.get(guild_id, False)
            status = "enabled" if is_loop[guild_id] else "disabled"
            await message.channel.send(f"🔄 Loop mode {status}")

        elif message.content.startswith('!!stop'):
            try:
                if guild_id in voice_clients:
                    voice_clients[guild_id].stop()
                    queues[guild_id].clear()  # Clear the queue
                    await message.channel.send("⏹️ Music stopped and queue cleared.")
                else:
                    await message.channel.send("❌ No music is playing.")
            except Exception as e:
                print(f"Error during stop: {str(e)}")
                traceback.print_exc()

        elif message.content.startswith('!!disconnect'):
            if guild_id in voice_clients:
                await voice_clients[guild_id].disconnect()
                voice_clients.pop(guild_id, None)
                queues.pop(guild_id, None)
                current_songs.pop(guild_id, None)
                is_loop.pop(guild_id, None)
                await message.channel.send("🔌 Disconnected from voice channel.")
            else:
                await message.channel.send("❌ Not connected to any voice channel.")
        elif message.content.startswith('!!help'):
            help_text = (
                "Offside BOT\n"
                "`Here are the commands you can use:\n"
                "- !!play <song name or URL> - Play a song\n"
                "- !!queue ------------------- Show the current queue\n"
                "- !!skip -------------------- Skip to the next song\n"
                "- !!clear ------------------- Clear the queue\n"
                "- !!pause ------------------- Pause the music\n"
                "- !!resume ------------------ Resume the music\n"
                "- !!loop -------------------- Toggle loop mode\n"
                "- !!stop -------------------- Stop the music and clear the queue\n"
                "- !!disconnect -------------- Disconnect from the voice channel\n"
                "\nDeveloped with ❤️ by @evangelion.xyz`"

            )
            await message.channel.send(help_text)

    @client.event
    async def on_voice_state_update(member, before, after):
        if member == client.user and after.channel is None:
            guild_id = before.channel.guild.id
            print('Bot disconnected. Clearing voice client and queue.')
            voice_clients.pop(guild_id, None)
            queues.pop(guild_id, None)
            current_songs.pop(guild_id, None)
            is_loop.pop(guild_id, None)
            if guild_id in disconnect_timers:
                disconnect_timers[guild_id].cancel()
                disconnect_timers.pop(guild_id, None)

    client.run(TOKEN)

if __name__ == "__main__":
    run_bot()