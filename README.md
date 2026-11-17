# mp3video

Aplicación en Rust para Windows con **interfaz gráfica nativa** (y soporte por línea de comandos) para renderizar lotes de archivos `.mp3` con una imagen estática a videos `.mp4` optimizados con FFmpeg, consultar datos de discos y compositores por código de catálogo (**VGMdb / MusicBrainz**) y subir los videos a **YouTube**.

## Uso con interfaz gráfica (GUI)

Hacé **doble clic en `mp3video.exe`** (o ejecutalo sin argumentos):

1. **Entrada y salida**: elegí o arrastrá una carpeta con `.mp3`, un archivo `.mp3` individual y una imagen estática opcional (`.png`, `.jpg`, `.jpeg`, `.webp`, `.bmp`). Si dejás la imagen vacía, busca automáticamente una imagen con el mismo nombre del tema o una portada común (`cover.jpg`, `portada.png`, etc.) en la carpeta.
2. **Metadatos del Álbum (VGMdb / MusicBrainz)**:
   - Ingresá el **código de catálogo** del disco (ej. `SQEX-10001`), un ID numérico de VGMdb o arrastrá un archivo `.json` de VGMdb, y presioná **🔍 Buscar Disco**.
   - Completa automáticamente el **Álbum**, **Compositores**, sello, fecha y lista de pistas. Si `vgmdb.info` no responde, consulta automáticamente **MusicBrainz** por código de catálogo y usa las etiquetas ID3 embebidas (`ffprobe`) como respaldo.
   - Podés editar manualmente el álbum, compositores y las plantillas de **Título** y **Descripción** usando variables: `{album}`, `{catalog}`, `{disc}`, `{track_num}`, `{disc_track}`, `{track_title}`, `{composers}`, `{arrangers}`, `{performers}`, `{publisher}`, `{release_date}`.
3. **Subida a YouTube**:
   - Seleccioná (o arrastrá) tu archivo `client_secret.json` de Google Cloud Console (credencial OAuth 2.0 tipo *Aplicación de escritorio* con *YouTube Data API v3* habilitada).
   - Presioná **🔑 Conectar YouTube** una sola vez para autorizar la cuenta en el navegador (los tokens se guardan en `youtube_tokens.json` y se renuevan automáticamente).
   - Elegí la privacidad (`No listado`, `Privado`, `Público`), activá si querés crear una **Playlist** del álbum y marcá **Subir a YouTube al terminar render** (o usá **☁ Subir MP4s ya renderizados** para subir videos existentes).
   - El historial de videos ya subidos se guarda en `uploaded_youtube.json` dentro de la carpeta de salida para poder reanudar lotes si se alcanza la cuota diaria de YouTube API.

## Uso por línea de comandos (CLI)

```powershell
# Consultar ficha del disco, compositores y pistas por código de catálogo
.\mp3video.exe --vgmdb SQEX-10001 --vgmdb-only

# Renderizar con imagen estática y fondo blur
.\mp3video.exe -i .\temas -m .\portada.png -o .\videos --fit blur

# Renderizar usando metadatos del catálogo y subir a YouTube creando una playlist
.\mp3video.exe -i .\temas -m .\portada.png -o .\videos --fit blur --vgmdb SQEX-10001 --youtube-upload --client-secret .\client_secret.json --youtube-privacy unlisted --youtube-playlist
```

### Opciones CLI principales

- `-i, --input <PATH>`: Carpeta con `.mp3` o archivo `.mp3` individual (por defecto: `.`).
- `-m, --image <PATH>`: Imagen estática por defecto.
- `-o, --output <DIR>`: Carpeta de salida (por defecto: `output`).
- `-r, --recursive`: Buscar `.mp3` en subcarpetas.
- `-j, --jobs <N>`: Cantidad de renders simultáneos.
- `--resolution <ANCHOxALTO>` / `--fit <contain|cover|blur|stretch>` / `--fps <N>` / `--crf <0-51>`.
- `--audio-bitrate <BITRATE>` / `--copy-audio` / `--overwrite` / `--ffmpeg-path <PATH>`.
- `--vgmdb <CODIGO>` / `--vgmdb-only` / `--vgmdb-url <URL>`.
- `--youtube-upload` / `--client-secret <PATH>` / `--youtube-privacy <unlisted|private|public>` / `--youtube-playlist`.
- `--title-template <TPL>` / `--desc-template <TPL>`.

## Compilación desde el código fuente

```powershell
cargo build --release
```
