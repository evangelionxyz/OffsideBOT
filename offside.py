import discord 
from discord.ext import commands
import asyncio
import os
import yt_dlp
from dotenv import load_dotenv
import traceback
from collections import deque

def run_bot():
    load_dotenv()

    queues = {}
    is_loop = {}
    current_songs = {}

    TOKEN = os.getenv('discord_token')
    client = discord.Client(intents=discord.Intents.all())

    voice_clients = {}
    yt_dl_options = {
        'format': 'bestaudio/best',
        'noplaylist': True,
        'nocheckcertificate': True,
        'ignoreerrors': False,
        'logtostderr': False,
        'quiet': True,
        'no_warnings': True,
        'extractaudio': True,
        'audioformat': 'mp3',
        'preferredcodec': 'mp3',
        'default_search': 'auto'
    }
    ytdl = yt_dlp.YoutubeDL(yt_dl_options)

    ffmpeg_options = {
        'options': '-vn',
        'before_options': '-reconnect 1 -reconnect_streamed 1 -reconnect_delay_max 5'
    }

    async def play_next_song(guild_id):
        """Play the next song in the queue"""
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

    @client.event
    async def on_ready():
        print(f'{client.user} is now jamming.')

    @client.event
    async def on_message(message):
        if message.author.bot:
            return

        guild_id = message.guild.id

        if message.content.startswith('//play'):
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

                query = message.content[len('//play '):].strip()
                
                try:
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

        elif message.content.startswith('//queue'):
            if guild_id in queues and queues[guild_id]:
                queue_list = "\n".join([
                    f"{i+1}. {song['title']}"
                    for i, song in enumerate(queues[guild_id])
                ])
                current = current_songs.get(guild_id, {}).get('title', 'Nothing')
                await message.channel.send(f"**Now Playing:** `{current}`\n\n**Queue:**\n{queue_list}")
            else:
                await message.channel.send("Queue is empty!")

        elif message.content.startswith('//skip'):
            if guild_id in voice_clients and voice_clients[guild_id].is_playing():
                voice_clients[guild_id].stop()  # This will trigger play_next_song
                await message.channel.send("⏭️ Skipped to next song.")
            else:
                await message.channel.send("❌ Nothing to skip.")

        elif message.content.startswith('//clear'):
            if guild_id in queues:
                queues[guild_id].clear()
                await message.channel.send("🗑️ Queue cleared.")
            else:
                await message.channel.send("❌ No queue to clear.")

        elif message.content.startswith('//pause'):
            try:
                if guild_id in voice_clients and voice_clients[guild_id].is_playing():
                    voice_clients[guild_id].pause()
                    await message.channel.send("⏸️ Music paused.")
                else:
                    await message.channel.send("❌ No music is playing.")
            except Exception as e:
                print(f"Error during pause: {str(e)}")
                traceback.print_exc()

        elif message.content.startswith('//resume'):
            try:
                if guild_id in voice_clients and voice_clients[guild_id].is_paused():
                    voice_clients[guild_id].resume()
                    await message.channel.send("▶️ Music resumed.")
                else:
                    await message.channel.send("❌ No music is paused.")
            except Exception as e:
                print(f"Error during resume: {str(e)}")
                traceback.print_exc()

        elif message.content.startswith('//loop'):
            is_loop[guild_id] = not is_loop.get(guild_id, False)
            status = "enabled" if is_loop[guild_id] else "disabled"
            await message.channel.send(f"🔄 Loop mode {status}")

        elif message.content.startswith('//stop'):
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

    @client.event
    async def on_voice_state_update(member, before, after):
        if member == client.user and after.channel is None:
            guild_id = before.channel.guild.id
            print('Bot disconnected. Clearing voice client and queue.')
            voice_clients.pop(guild_id, None)
            queues.pop(guild_id, None)
            current_songs.pop(guild_id, None)
            is_loop.pop(guild_id, None)

    client.run(TOKEN)

if __name__ == "__main__":
    run_bot()