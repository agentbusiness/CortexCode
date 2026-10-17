FROM python:3.12
WORKDIR /srv
COPY . /srv
RUN pip install -r app/requirements.txt
EXPOSE 8000
CMD python -m app.main
