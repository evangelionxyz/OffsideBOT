import discord 
from discord.ext import commands
import asyncio
import os
import yt_dlp
from dotenv import load_dotenv

def run_bot():
    load_dotenv()

    TOKEN = os.getenv('discord_token')
    client = discord.Client(intents=discord.Intents.all())

    voice_clients = {}
    yt_dl_options = {
        'format': 'bestaudio/best',
        'preferredcodec': 'mp3',
        'extractaudio': True,
        'default_search': 'auto',
        'noplaylist': True,  # skip playlists
        'playlistend': 1,    # only process first item in any list
        'quiet': True        # reduce console noise
    }
    ytdl = yt_dlp.YoutubeDL(yt_dl_options)

    ffmpeg_options = {'options': '-vn'}

    @client.event
    async def on_ready():
        print(f'{client.user} is now jamming.')
    
    @client.event
    async def on_message(message):
        if message.author == client.user:
            return

        if message.content.startswith('//play'):
            try:
                if not message.author.voice:
                    await message.channel.send("You need to be in a voice channel!")
                    return

                if len(message.content.split()) < 2:
                    await message.channel.send("Please provide a song name or URL.")
                    return
                query = ' '.join(message.content.split()[1:])

                voice_client = voice_clients.get(message.guild.id)
                if not voice_client:
                    voice_client = await message.author.voice.channel.connect()
                    voice_clients[message.guild.id] = voice_client

                loop = asyncio.get_event_loop()
                data = await loop.run_in_executor(None, lambda: ytdl.extract_info(query, download=False))

                if 'entries' in data:
                    data = data['entries'][0]

                song = data['url']
                title = data.get('title', 'Unknown Track')

                player = discord.FFmpegPCMAudio(song, **ffmpeg_options)
                voice_client.play(player)
                await message.channel.send(f"Now playing: **{title}**")

            except Exception as e:
                await message.channel.send("Error processing the request.")
                print(f"Error: {e}")

        if message.content.startswith('//stop'):
            try:
                voice_clients[message.guild.id].stop()
            except Exception as e:
                print(e)

        if message.content.startswith('//pause'):
            try:
                voice_clients[message.guild.id].pause()
            except Exception as e:
                print(e)

    client.run(TOKEN)

if __name__ == "__main__":
    run_bot()