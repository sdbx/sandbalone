package models

import "time"

type GameServer struct {
	Name     string    `json:"name"`
	Addr     string    `json:"addr"`
	Rooms    []Room    `json:"rooms"`
	LastPing time.Time `json:"last_ping"`
}

type Room struct {
	ID        string    `json:"id"`
	CreatedAt time.Time `json:"created_at"`
	Name      string    `json:"name"`
}
