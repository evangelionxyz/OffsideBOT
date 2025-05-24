# Offside BOT

### Ubuntu 24.04 
``` bash
sudo apt update
sudo apt install ffmpeg
pip install discord yt-dlp dotenv spotipy
```

## Discord token
Go to [Discord Developer Application](https://discord.com/developers/applications)
<br>
Choose your `Application` then choose `Bot` section and copy the TOKEN from the token section (you may need to reset / regenerate the token)
<br>
Create `token.txt` and just paste directly to the file

## Youtube Cookies
Install cookies exporter e.g: [Microsoft Edge Extension](https://microsoftedge.microsoft.com/addons/detail/export-cookies-file/hbglikhfdcfhdfikmocdflffaecbnedo?hl=en-GB)
<br>
Open youtube.com and then export the cookies with the extension name it `cookies.txt`
<br>
paste it next to `offside.py` file

## Spotify

Go [Spotify Developer](https://developer.spotify.com/dashboard)<br>
Create your application and get the CLIENT_ID and CLIENT_SECRET
<br>
Create `.env` file paste them there
```
SPOTIPY_CLIENT_ID=your_client_id
SPOTIPY_CLIENT_SECRET=your_client_secret
SPOTIPY_REDIRECT_URI=http://127.0.0.1:8000/callback
```