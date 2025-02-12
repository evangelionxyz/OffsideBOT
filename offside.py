import discord 
from discord.ext import commands
import asyncio
import os
import yt_dlp
from dotenv import load_dotenv

def run_bot():
    load_dotenv()

    current_song = ''
    current_guild_id = ''

    TOKEN = os.getenv('discord_token')
    client = discord.Client(intents=discord.Intents.all())

    voice_clients = {}
    yt_dl_options = {
        'format': "bestaudio/best",
        'noplaylist': True,
        'extract_flat': True
    }
    ytdl = yt_dlp.YoutubeDL(yt_dl_options)

    ffmpeg_options = {
        'options': '-vn',
        'before_options': '-reconnect 1 -reconnect_streamed 1 - reconnect_delay_max 5'
    }


    @client.event
    async def on_ready():
        print(f'{client.user} is now jamming.')

   
    def restart_song(error):
        if error:
            print(f'FFmpeg crashed: {error}')
        else:
            print(f'FFmpeg finished playing.')

        # restart song if it wasn't manually stopped
        if current_song and voice_clients.get(current_guild_id):
            print("Restarting song...")
            voice_clients[current_guild_id].play(
                discord.FFmpegPCMAudio(current_song, **ffmpeg_options), after=restart_song
            )
    
    @client.event
    async def on_message(message):
        if message.author.bot:
            return # ignore bot messages
        
        if message.content.startswith('//play'):
            try:
                if message.author.voice is None or message.author.voice.channel is None:
                    await message.channel.send('You need to be in a voice channel!')
                    return
                
                if message.guild.id not in voice_clients:
                    voice_client = await message.author.voice.channel.connect()
                    voice_clients[voice_client.guild.id] = voice_client

                query = message.content[len('//play '):].strip()

                if query.startswith('http'):
                    url = query # direct URL provided
                    loop = asyncio.get_event_loop()
                    data = await loop.run_in_executor(None, lambda: ytdl.extract_info(query, download=False))
                    if 'url' in data:
                        current_song = data['url']
                        title = data.get('title', 'Unknown Title')
                        await message.channel.send(f"🎶 Now playing: `{title}`")
                    else:
                        await message.channel.send('❌ Could not extract audio URL.')
                        return
                else:
                    # handle search query
                    await message.channel.send(f'🔍 Searching for `{query}` on YouTube...')

                    search_data = await asyncio.get_event_loop().run_in_executor(
                        None, 
                        lambda: ytdl.extract_info(f"ytsearch1:{query}", download=False)
                    )

                    if not search_data.get('entries'):
                        await message.channel.send('❌ No results found.')
                        return

                    video_url = search_data['entries'][0]['url']
                    title = search_data['entries'][0].get('title', 'Unknown Title')

                    # get the playable URL
                    data = await asyncio.get_event_loop().run_in_executor(
                        None,
                        lambda: ytdl.extract_info(video_url, download=False)
                    )

                    current_song = data['url']

                    if voice_clients[message.guild.id].is_playing():
                        voice_clients[message.guild.id].stop()

                    voice_clients[message.guild.id].play(
                        discord.FFmpegPCMAudio(current_song, **ffmpeg_options),
                        after=restart_song
                    )

                # Extract audio and play
                loop = asyncio.get_event_loop()
                data = await loop.run_in_executor(None, lambda: ytdl.extract_info(url, download=False))
                current_song = data['url']
                current_guild_id = message.guild.id

                voice_clients[message.guild.id].play(
                    discord.FFmpegPCMAudio(current_song, **ffmpeg_options), 
                    after=lambda e: 
                    print(f'FFmpeg process stopped: {e}')
                )

            except Exception as e:
                print(e)
                await message.channel.send("⚠️ An error occurred while playing the song.")

        if message.content.startswith('//pause'):
            try:
                if message.guild.id in voice_clients and voice_clients[message.guild.id].is_playing():
                    voice_clients[message.guild.id].pause()
                    await message.channel.send("⏸️ Music paused.")
                else:
                    await message.channel.send("❌ No music is playing.")
            except Exception as e:
                print(e)

    @client.event
    async def on_voice_state_update(member, before, after):
        if member == client.user and after.channel is None:
            print('Bot disconnected. Clearing voice client.')
            voice_clients.pop(member.guild.id, None)

    client.run(TOKEN)



if __name__ == "__main__":
    run_bot()