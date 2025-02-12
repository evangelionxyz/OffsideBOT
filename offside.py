import discord 
from discord.ext import commands
import asyncio
import os
import yt_dlp
from dotenv import load_dotenv
import traceback

def run_bot():
    load_dotenv()

    current_song = None
    current_guild_id = None
    is_loop = False  # control flag for looping

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

    @client.event
    async def on_ready():
        print(f'{client.user} is now jamming.')

    def play_next(error, guild_id, song_url, song_title):
        """Handle song completion and restart if looping is enabled"""
        if error:
            print(f'FFmpeg error: {error}')
        
        # check if the bot is still in a voice channel and looping is enabled
        if guild_id in voice_clients and is_loop:
            voice_client = voice_clients[guild_id]
            if voice_client.is_connected() and not voice_client.is_playing():
                print(f"Restarting song: {song_title}")
                voice_client.play(
                    discord.FFmpegPCMAudio(song_url, **ffmpeg_options),
                    after=lambda e: play_next(e, guild_id, song_url, song_title)
                )

    @client.event
    async def on_message(message):
        if message.author.bot:
            return

        if message.content.startswith('//play') or message.content.startswith('//p'):
            try:
                if message.author.voice is None or message.author.voice.channel is None:
                    await message.channel.send('You need to be in a voice channel!')
                    return
                
                # connect to voice channel if not already connected
                if message.guild.id not in voice_clients:
                    voice_client = await message.author.voice.channel.connect()
                    voice_clients[voice_client.guild.id] = voice_client

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

                    song_url = video['url']
                    song_title = video['title']
                    
                    # stop current playback if any
                    if voice_clients[message.guild.id].is_playing():
                        voice_clients[message.guild.id].stop()

                    voice_clients[message.guild.id].play(
                        discord.FFmpegPCMAudio(song_url, **ffmpeg_options),
                        after=lambda e: play_next(e, message.guild.id, song_url, song_title)
                    )
                    
                    await message.channel.send(f"🎶 Now playing: `{song_title}`")

                except Exception as e:
                    print(f"Error during playback setup: {str(e)}")
                    traceback.print_exc()
                    await message.channel.send(f"⚠️ Error: {str(e)}")

            except Exception as e:
                print(f"Outer error: {str(e)}")
                traceback.print_exc()
                await message.channel.send(f"⚠️ An error occurred: {str(e)}")

        elif message.content.startswith('//pause'):
            try:
                if message.guild.id in voice_clients and voice_clients[message.guild.id].is_playing():
                    voice_clients[message.guild.id].pause()
                    await message.channel.send("⏸️ Music paused.")
                else:
                    await message.channel.send("❌ No music is playing.")
            except Exception as e:
                print(f"Error during pause: {str(e)}")
                traceback.print_exc()

        elif message.content.startswith('//resume'):
            try:
                if message.guild.id in voice_clients and voice_clients[message.guild.id].is_paused():
                    voice_clients[message.guild.id].resume()
                    await message.channel.send("▶️ Music resumed.")
                else:
                    await message.channel.send("❌ No music is paused.")
            except Exception as e:
                print(f"Error during resume: {str(e)}")
                traceback.print_exc()

        elif message.content.startswith('//loop'):
            nonlocal is_loop
            is_loop = not is_loop
            status = "enabled" if is_loop else "disabled"
            await message.channel.send(f"🔄 Loop mode {status}")

        elif message.content.startswith('//stop'):
            try:
                if message.guild.id in voice_clients:
                    is_loop = False
                    voice_clients[message.guild.id].stop()
                    await message.channel.send("⏹️ Music stopped.")
                else:
                    await message.channel.send("❌ No music is playing.")
            except Exception as e:
                print(f"Error during stop: {str(e)}")
                traceback.print_exc()

    @client.event
    async def on_voice_state_update(member, before, after):
        if member == client.user and after.channel is None:
            print('Bot disconnected. Clearing voice client.')
            voice_clients.pop(member.guild.id, None)

    client.run(TOKEN)

if __name__ == "__main__":
    run_bot()